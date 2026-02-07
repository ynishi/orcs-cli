//! ORCS Application.
//!
//! High-level application wrapper that integrates:
//!
//! - [`OrcsEngine`] - Core execution engine
//! - [`EchoWithHilComponent`] - Echo with HIL integration (via ChannelRunner)
//! - [`InputParser`] - stdin command parsing
//! - [`ConsoleRenderer`] - User output display
//!
//! # Input Flow
//!
//! ```text
//! stdin → handle_input() → InputCommand
//!                            │
//!          ┌─────────────────┴─────────────────┐
//!          │                                   │
//!          ▼                                   ▼
//!    App-local                             IOInput
//!    (Quit/Pause/Resume/Help)          (All other input)
//!          │                                   │
//!          │                                   ▼
//!          │                            ClientRunner
//!          │                       io_bridge.recv_input()
//!          │                                   │
//!          │                     ┌─────────────┴─────────────┐
//!          │                     │                           │
//!          │                     ▼                           ▼
//!          │              Signal (y/n/veto)           User message
//!          │                     │                   handle_user_message()
//!          │                     │                           │
//!          │                     ▼                           ▼
//!          │            Signal broadcast          Component.on_request()
//!          │                     │                           │
//!          └─────────────────────┴───────────────────────────┘
//!                                │
//!                                ▼
//!                    IOOutput → handle_io_output()
//! ```
//!
//! # Architecture
//!
//! The EchoWithHilComponent runs inside ClientRunner (parallel tokio task).
//! App manages UI state (pending_approval) locally, updated via IOOutput.
//!
//! ## Input Routing
//!
//! | Input Type | Route | Handler |
//! |------------|-------|---------|
//! | Quit/Pause/Resume/Help | App-local | LoopControl |
//! | Approve/Reject/Veto | IOInput → ClientRunner | Signal broadcast |
//! | User message (Unknown) | IOInput → ClientRunner | Component.on_request() |
//!
//! # Example
//!
//! ```ignore
//! use orcs_app::{OrcsApp, NoOpResolver};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut app = OrcsApp::builder(NoOpResolver).build().await?;
//!     app.run_interactive().await?;
//!     Ok(())
//! }
//! ```

use crate::AppError;
use mlua::Lua;
use orcs_component::{ChildConfig, Component, RunnableChild, SpawnError};
use orcs_event::Signal;
use orcs_lua::{LuaChild, ScriptLoader};
use orcs_runtime::auth::{DefaultGrantStore, DefaultPolicy, GrantPolicy, PermissionChecker};
use orcs_runtime::io::{ConsoleRenderer, InputParser};
use orcs_runtime::LuaChildLoader;
use orcs_runtime::{
    ChannelConfig, ConfigResolver, IOInput, IOInputHandle, IOOutput, IOOutputHandle, IOPort,
    InputCommand, InputContext, LocalFileStore, OrcsConfig, OrcsEngine, Session, SessionAsset,
    World,
};
use orcs_types::{ChannelId, Principal, PrincipalId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

/// LuaChildLoader implementation for spawning Lua children.
struct AppLuaChildLoader {
    sandbox: Arc<dyn orcs_runtime::sandbox::SandboxPolicy>,
}

impl LuaChildLoader for AppLuaChildLoader {
    fn load(&self, config: &ChildConfig) -> Result<Box<dyn RunnableChild>, SpawnError> {
        // Get script from config (prefer inline over path)
        let script = config
            .script_inline
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no script_inline in config".into()))?;

        // Create new Lua instance for this child
        let lua = Lua::new();
        let lua_arc = Arc::new(Mutex::new(lua));

        // Create LuaChild from script
        let child = LuaChild::from_script(lua_arc, script, Arc::clone(&self.sandbox))
            .map_err(|e| SpawnError::Internal(format!("failed to load Lua child: {}", e)))?;

        Ok(Box::new(child))
    }
}

/// Pending approval state.
///
/// Tracks the last approval request for "y" / "n" without explicit ID.
#[derive(Debug, Clone)]
struct PendingApproval {
    /// Approval ID (UUID).
    id: String,
}

/// Control flow for the main input loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopControl {
    /// Continue processing input.
    Continue,
    /// Exit the loop normally (quit command).
    Exit,
    /// Pause and save session before exit.
    Pause,
}

/// ORCS Application.
///
/// Integrates the engine with HIL, input handling, and output display.
pub struct OrcsApp {
    /// Loaded configuration (merged from all sources).
    config: OrcsConfig,
    /// Configuration resolver (retained for hot-reload).
    resolver: Box<dyn ConfigResolver>,
    engine: OrcsEngine,
    /// Principal for signal creation.
    principal: Principal,
    /// Console renderer for output display.
    renderer: ConsoleRenderer,
    /// Handle for sending input to ClientRunner via IOPort.
    ///
    /// Primary input channel for View → ClientRunner communication.
    /// All user input flows through this channel.
    io_input: IOInputHandle,
    /// Handle for receiving IO output from ClientRunner.
    io_output: IOOutputHandle,
    /// Pending approval (for "y" / "n" without explicit ID).
    pending_approval: Option<PendingApproval>,
    /// Session store for persistence.
    store: LocalFileStore,
    /// Current session asset.
    session: SessionAsset,
    /// Whether this session was resumed from storage.
    is_resumed: bool,
    /// Number of components restored (if resumed).
    restored_count: usize,
    /// Component name → ChannelId routing table.
    ///
    /// Used by `@component message` syntax for targeted delivery.
    component_routes: HashMap<String, ChannelId>,
}

impl OrcsApp {
    /// Creates a new builder for OrcsApp.
    ///
    /// The resolver is required and controls how configuration is loaded.
    /// The application retains the resolver for hot-reload support.
    #[must_use]
    pub fn builder(resolver: impl ConfigResolver + 'static) -> OrcsAppBuilder {
        OrcsAppBuilder::new(resolver)
    }

    /// Returns a reference to the engine.
    #[must_use]
    pub fn engine(&self) -> &OrcsEngine {
        &self.engine
    }

    /// Returns a mutable reference to the engine.
    pub fn engine_mut(&mut self) -> &mut OrcsEngine {
        &mut self.engine
    }

    /// Returns a reference to the loaded configuration.
    #[must_use]
    pub fn config(&self) -> &OrcsConfig {
        &self.config
    }

    /// Re-resolves configuration from all sources (hot-reload).
    ///
    /// Calls the retained `ConfigResolver::resolve()` to get a fresh
    /// config reflecting any file/env changes since last resolution.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Config`] if resolution fails.
    pub fn reload_config(&mut self) -> Result<(), AppError> {
        self.config = self
            .resolver
            .resolve()
            .map_err(|e| AppError::Config(e.to_string()))?;
        Ok(())
    }

    /// Returns the console renderer.
    #[must_use]
    pub fn renderer(&self) -> &ConsoleRenderer {
        &self.renderer
    }

    /// Returns the pending approval ID.
    #[must_use]
    pub fn pending_approval_id(&self) -> Option<&str> {
        self.pending_approval.as_ref().map(|p| p.id.as_str())
    }

    /// Returns the current session ID.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session.id
    }

    /// Returns whether this session was resumed from storage.
    #[must_use]
    pub fn is_resumed(&self) -> bool {
        self.is_resumed
    }

    /// Returns the number of components restored (if resumed).
    #[must_use]
    pub fn restored_count(&self) -> usize {
        self.restored_count
    }

    /// Saves the current session to storage.
    ///
    /// Collects component snapshots and persists the session.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Storage`] if saving fails.
    pub async fn save_session(&mut self) -> Result<(), AppError> {
        self.engine
            .save_session(&self.store, &mut self.session)
            .await?;
        Ok(())
    }

    /// Loads a session from storage and restores component state.
    ///
    /// # Arguments
    ///
    /// * `session_id` - The session ID to load
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Storage`] if loading fails.
    pub async fn load_session(&mut self, session_id: &str) -> Result<(), AppError> {
        self.session = self.engine.load_session(&self.store, session_id).await?;
        Ok(())
    }

    /// Runs the application in interactive mode.
    ///
    /// Uses parallel execution infrastructure:
    /// - WorldManager for concurrent World access
    /// - ChannelRunner per channel for parallel execution
    /// - Event injection via channel handles
    ///
    /// Reads commands from stdin and processes them until quit.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Io`] on stdin read errors.
    pub async fn run_interactive(&mut self) -> Result<(), AppError> {
        let io_id = self.engine.io_channel();
        tracing::info!("Starting interactive mode (IO channel: {})", io_id);

        self.engine.start();
        self.renderer.render_output(&IOOutput::info(
            "Interactive mode started. Type 'q' to quit, 'help' for commands.",
        ));

        // Display session status with restore information
        if self.is_resumed {
            self.renderer.render_output(&IOOutput::info(format!(
                "Resumed session: {} ({} component(s) restored)",
                self.session.id, self.restored_count
            )));
        } else {
            self.renderer
                .render_output(&IOOutput::info(format!("Session ID: {}", self.session.id)));
        }

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        let mut should_save = false;

        loop {
            // Poll engine once
            if !self.engine.is_running() {
                break;
            }

            // Check for stdin input and IO output
            tokio::select! {
                line_result = reader.next_line() => {
                    match line_result {
                        Ok(Some(line)) => {
                            match self.handle_input(&line) {
                                LoopControl::Continue => {}
                                LoopControl::Exit => break,
                                LoopControl::Pause => {
                                    should_save = true;
                                    break;
                                }
                            }
                        }
                        Ok(None) => {
                            // EOF
                            tracing::debug!("stdin EOF");
                            break;
                        }
                        Err(e) => {
                            tracing::error!("stdin error: {}", e);
                            return Err(AppError::Io(e));
                        }
                    }
                }
                Some(io_out) = self.io_output.recv() => {
                    // Display IO output from ClientRunner
                    self.handle_io_output(io_out);
                }
                () = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                    // Yield to allow engine polling
                }
            }

            tokio::task::yield_now().await;
        }

        self.engine.stop();

        // Save session if paused
        if should_save {
            self.renderer
                .render_output(&IOOutput::info("Saving session..."));
            self.save_session().await?;
            self.renderer.render_output(&IOOutput::info(format!(
                "Session saved: {}",
                self.session.id
            )));
            self.renderer.render_output(&IOOutput::info(format!(
                "Resume with: orcs --resume {}",
                self.session.id
            )));
        }

        self.renderer
            .render_output(&IOOutput::info("Shutting down..."));
        Ok(())
    }

    /// Handles a single line of input.
    ///
    /// # Input Routing
    ///
    /// - **Control signals** (Approve/Reject/Veto): → IOInput経由でClientRunner
    /// - **App-local commands** (Quit/Pause/Resume/Help): → App内で処理
    /// - **Component messages** (`@name msg`): → Engine.inject_event → 特定Channel
    /// - **User messages** (Unknown): → IOInput経由でClientRunner → 全Component
    ///
    /// Returns control flow instruction for the main loop.
    fn handle_input(&mut self, line: &str) -> LoopControl {
        let cmd = InputParser.parse(line);

        match &cmd {
            InputCommand::Quit => {
                tracing::info!("Quit requested");
                return LoopControl::Exit;
            }
            InputCommand::Veto => {
                tracing::warn!("Veto signal sent");
                // Send via IOInput (ClientRunner will convert to Signal)
                self.send_io_input(line);
                return LoopControl::Exit;
            }
            InputCommand::Approve { approval_id } => {
                self.handle_approve_via_io(line, approval_id.as_deref());
            }
            InputCommand::Reject {
                approval_id,
                reason: _,
            } => {
                self.handle_reject_via_io(line, approval_id.as_deref());
            }
            InputCommand::Pause => {
                tracing::info!("Pause requested");
                return LoopControl::Pause;
            }
            InputCommand::Resume => {
                // Resume is handled at startup via --resume flag, not during interactive mode
                self.renderer.render_output(&IOOutput::info(
                    "Resume is handled at startup. Use: orcs --resume",
                ));
            }
            InputCommand::Steer { message } => {
                self.renderer
                    .render_output(&IOOutput::info(format!("Steer: {message}")));
                // TODO: Implement steer
            }
            InputCommand::Empty => {
                // Blank line - ignore
            }
            InputCommand::ComponentMessage { target, message } => {
                self.handle_component_message(target, message);
            }
            InputCommand::Unknown { input } => {
                if input == "help" {
                    self.show_help();
                } else if !input.is_empty() {
                    // Send user message to ClientRunner via IOInput
                    self.handle_echo_input(input.as_str());
                }
            }
        }

        LoopControl::Continue
    }

    /// Sends raw input to ClientRunner via IOInput.
    ///
    /// Used for control signals that ClientRunner should convert to Signal.
    fn send_io_input(&self, line: &str) {
        let context = self.build_input_context();
        let io_input = IOInput::line_with_context(line, context);

        if let Err(e) = self.io_input.try_send(io_input) {
            tracing::warn!("Failed to send IOInput: {:?}", e);
        }
    }

    /// Builds InputContext with current pending approval ID.
    fn build_input_context(&self) -> InputContext {
        match &self.pending_approval {
            Some(pending) => InputContext::with_approval_id(&pending.id),
            None => InputContext::empty(),
        }
    }

    /// Handles user message input by sending to ClientRunner via IOInput.
    ///
    /// The message is sent to ClientRunner, which forwards it to the Component.
    /// If the Component requires HIL approval, it will send back an approval
    /// request via IOOutput, which App will receive and track.
    fn handle_echo_input(&mut self, input: &str) {
        // Send user message via IOInput (no context needed for messages)
        let io_input = IOInput::line(input);

        if let Err(e) = self.io_input.try_send(io_input) {
            self.renderer
                .render_output(&IOOutput::error(format!("Failed to send message: {e:?}")));
        }
    }

    /// Routes a message to a specific component via `@component message`.
    ///
    /// Looks up the component name in `component_routes` and injects
    /// a UserInput event directly to the target channel.
    fn handle_component_message(&mut self, target: &str, message: &str) {
        let channel_id = match self.component_routes.get(target) {
            Some(id) => *id,
            None => {
                let available: Vec<&str> =
                    self.component_routes.keys().map(|k| k.as_str()).collect();
                self.renderer.render_output(&IOOutput::error(format!(
                    "Unknown component: @{}. Available: {}",
                    target,
                    available.join(", ")
                )));
                return;
            }
        };

        let event = orcs_runtime::Event {
            category: orcs_event::EventCategory::UserInput,
            operation: "input".to_string(),
            source: orcs_types::ComponentId::builtin("app"),
            payload: serde_json::json!({ "message": message }),
        };

        match self.engine.inject_event(channel_id, event) {
            Ok(()) => {
                tracing::debug!("Routed @{} to channel {}", target, channel_id);
            }
            Err(e) => {
                self.renderer.render_output(&IOOutput::error(format!(
                    "Failed to route to @{}: {}",
                    target, e
                )));
            }
        }
    }

    /// Handles an approve command by broadcasting Signal::Approve.
    ///
    /// Broadcasts the signal to all channels via the Engine's signal bus.
    /// The shell's ChannelRunner receives it and dispatches to on_signal.
    fn handle_approve_via_io(&mut self, _line: &str, explicit_id: Option<&str>) {
        let approval_id = explicit_id
            .map(String::from)
            .or_else(|| self.pending_approval.as_ref().map(|p| p.id.clone()));

        if let Some(id) = approval_id {
            // Broadcast signal to all channels (shell receives via signal_rx)
            let signal = Signal::approve(&id, self.principal.clone());
            self.engine.signal(signal);
            tracing::info!(approval_id = %id, "Approved");
            self.renderer.render_output(&IOOutput::approved(&id));

            // Clear pending approval
            if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(id.as_str()) {
                self.pending_approval = None;
            }
        } else {
            self.renderer
                .render_output(&IOOutput::warn("No pending approval. Use: y <id>"));
        }
    }

    /// Handles a reject command by broadcasting Signal::Reject.
    ///
    /// Broadcasts the signal to all channels via the Engine's signal bus.
    fn handle_reject_via_io(&mut self, _line: &str, explicit_id: Option<&str>) {
        let approval_id = explicit_id
            .map(String::from)
            .or_else(|| self.pending_approval.as_ref().map(|p| p.id.clone()));

        if let Some(id) = approval_id {
            // Broadcast signal to all channels
            let signal = Signal::reject(&id, None, self.principal.clone());
            self.engine.signal(signal);
            tracing::info!(approval_id = %id, "Rejected");
            self.renderer.render_output(&IOOutput::rejected(&id, None));

            // Clear pending approval
            if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(id.as_str()) {
                self.pending_approval = None;
            }
        } else {
            self.renderer
                .render_output(&IOOutput::warn("No pending approval. Use: n <id> [reason]"));
        }
    }

    /// Shows help text.
    fn show_help(&self) {
        self.renderer.render_output(&IOOutput::info("Commands:"));
        self.renderer.render_output(&IOOutput::info(
            "  y [id]         - Approve pending request",
        ));
        self.renderer
            .render_output(&IOOutput::info("  n [id] [reason]- Reject pending request"));
        self.renderer
            .render_output(&IOOutput::info("  p / pause      - Pause execution"));
        self.renderer
            .render_output(&IOOutput::info("  r / resume     - Resume execution"));
        self.renderer
            .render_output(&IOOutput::info("  s <message>    - Steer with instruction"));
        self.renderer
            .render_output(&IOOutput::info("  q / quit       - Quit application"));
        self.renderer
            .render_output(&IOOutput::info("  veto / stop    - Emergency stop"));
    }

    /// Handles IO output from ClientRunner.
    ///
    /// Updates pending_approval when an approval request is received.
    fn handle_io_output(&mut self, io_out: IOOutput) {
        // Store as pending approval for "y" / "n" without ID
        if let IOOutput::ShowApprovalRequest { ref id, .. } = io_out {
            self.pending_approval = Some(PendingApproval { id: id.clone() });
        }

        // Delegate rendering to ConsoleRenderer
        self.renderer.render_output(&io_out);
    }
}

/// Builder for [`OrcsApp`].
///
/// Accepts a [`ConfigResolver`] that encapsulates all config resolution
/// logic (file loading, env vars, CLI overrides). The builder only
/// handles launch parameters (e.g., session resume).
///
/// # Example
///
/// ```ignore
/// use orcs_runtime::{ConfigResolver, OrcsConfig, ConfigError};
///
/// struct MyResolver;
/// impl ConfigResolver for MyResolver {
///     fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
///         Ok(OrcsConfig::default())
///     }
/// }
///
/// let app = OrcsApp::builder(MyResolver)
///     .build()
///     .await?;
/// ```
pub struct OrcsAppBuilder {
    /// Configuration resolver (owned, moved into OrcsApp).
    resolver: Box<dyn ConfigResolver>,
    /// Session ID to resume from (launch parameter).
    resume_session_id: Option<String>,
    /// Sandbox policy for file operations.
    sandbox: Option<Arc<dyn orcs_runtime::sandbox::SandboxPolicy>>,
}

impl OrcsAppBuilder {
    /// Creates a new builder with the given resolver.
    #[must_use]
    pub fn new(resolver: impl ConfigResolver + 'static) -> Self {
        Self {
            resolver: Box::new(resolver),
            resume_session_id: None,
            sandbox: None,
        }
    }

    /// Sets the sandbox policy for file operations.
    ///
    /// Required before calling [`build()`](Self::build).
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: Arc<dyn orcs_runtime::sandbox::SandboxPolicy>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Sets a session ID to resume from.
    #[must_use]
    pub fn resume(mut self, session_id: impl Into<String>) -> Self {
        self.resume_session_id = Some(session_id.into());
        self
    }

    /// Builds the application.
    ///
    /// Resolves configuration via the provided [`ConfigResolver`],
    /// creates the World, Engine, and spawns component runners.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Config`] if configuration resolution fails.
    pub async fn build(self) -> Result<OrcsApp, AppError> {
        // Resolve configuration (all layers handled by the resolver)
        let config = self
            .resolver
            .resolve()
            .map_err(|e| AppError::Config(e.to_string()))?;

        // Sandbox is required for file operations
        let sandbox = self.sandbox.ok_or_else(|| {
            AppError::Config("sandbox policy not set (use .with_sandbox())".into())
        })?;

        // Create session store
        let session_path = config.paths.session_dir_or_default();
        let store = LocalFileStore::new(session_path)
            .map_err(|e| AppError::Config(format!("Failed to create session store: {e}")))?;

        // Create World with IO channel and channels for components
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());
        let claude_channel = world.create_channel(ChannelConfig::default());
        let subagent_channel = world.create_channel(ChannelConfig::default());
        let agent_mgr_channel = world.create_channel(ChannelConfig::default());
        let shell_channel = world.create_channel(ChannelConfig::default());
        let tool_channel = world.create_channel(ChannelConfig::default());

        // Create engine with IO channel (required)
        let mut engine = OrcsEngine::new(world, io);

        // Load user scripts from configured directories (auto_load)
        if config.scripts.auto_load {
            let script_dirs = config.scripts.resolve_dirs(Some(sandbox.root()));
            if !script_dirs.is_empty() {
                let script_loader = ScriptLoader::new(Arc::clone(&sandbox))
                    .with_paths(script_dirs)
                    .without_embedded_fallback();
                let result = script_loader.load_all();

                // Capture counts before consuming
                let loaded_count = result.loaded_count();
                let warning_count = result.warning_count();

                // Log warnings but continue
                for warn in &result.warnings {
                    tracing::warn!(
                        path = %warn.path.display(),
                        error = %warn.error,
                        "Failed to load user script"
                    );
                }

                // Spawn each loaded component on its own channel
                for (name, component) in result.loaded {
                    let world_ref = engine.world_read();
                    let channel_id = {
                        let mut w = world_ref.blocking_write();
                        w.create_channel(ChannelConfig::default())
                    };
                    let component_id = component.id().clone();
                    engine.spawn_runner_with_emitter(channel_id, Box::new(component), None);
                    tracing::info!(
                        name = %name,
                        channel = %channel_id,
                        component = %component_id.fqn(),
                        "User script loaded"
                    );
                }

                if loaded_count > 0 {
                    tracing::info!(
                        "Loaded {} user script(s), {} warning(s)",
                        loaded_count,
                        warning_count
                    );
                }
            }
        }

        // Create IO port for ClientRunner
        let (io_port, io_input, io_output) = IOPort::with_defaults(io);
        let principal = Principal::User(PrincipalId::new());

        // Spawn ClientRunner for IO channel (no component - bridge only)
        let (_io_handle, io_event_tx) = engine.spawn_client_runner(io, io_port, principal.clone());
        tracing::info!("ClientRunner spawned: channel={} (IO bridge)", io);

        // Create shared auth context for all runners
        // Session starts elevated for interactive use (user is present)
        let auth_session: Arc<Session> =
            Arc::new(Session::new(principal.clone()).elevate(std::time::Duration::from_secs(3600)));
        let auth_checker: Arc<dyn PermissionChecker> = Arc::new(DefaultPolicy);
        // Grant stores: セッション中の HIL grant を保持する。
        // elevated runners は共有 store、shell は独自 store（non-elevated なので
        // HIL approval → grant_command() が実際に使われる唯一のコンポーネント）。
        // TODO: 将来的に Grant は Component レベルで管理すべき
        // （Grant 自体は Domain だが HIL フローは Component の責務）。
        let auth_grants: Arc<dyn GrantPolicy> = Arc::new(DefaultGrantStore::new());
        let shell_grants: Arc<dyn GrantPolicy> = Arc::new(DefaultGrantStore::new());
        tracing::info!(
            "Auth context created: principal={:?}, elevated={}",
            auth_session.principal(),
            auth_session.is_elevated()
        );

        // Spawn ChannelRunner for claude_cli with output routed to IO channel
        let lua_component = ScriptLoader::load_embedded("claude_cli", Arc::clone(&sandbox))
            .map_err(|e| AppError::Config(format!("Failed to load claude_cli script: {e}")))?;
        let component_id = lua_component.id().clone();
        let _claude_handle = engine.spawn_runner_full_auth(
            claude_channel,
            Box::new(lua_component),
            Some(io_event_tx.clone()),
            None, // No child spawner for claude_cli
            Arc::clone(&auth_session),
            Arc::clone(&auth_checker),
            Arc::clone(&auth_grants),
        );
        tracing::info!(
            "ChannelRunner spawned: channel={}, component={} (auth enabled)",
            claude_channel,
            component_id.fqn()
        );

        // Spawn ChannelRunner for subagent with output routed to IO channel
        let subagent_component = ScriptLoader::load_embedded("subagent", Arc::clone(&sandbox))
            .map_err(|e| AppError::Config(format!("Failed to load subagent script: {e}")))?;
        let subagent_id = subagent_component.id().clone();
        let _subagent_handle = engine.spawn_runner_full_auth(
            subagent_channel,
            Box::new(subagent_component),
            Some(io_event_tx.clone()),
            None, // No child spawner for subagent
            Arc::clone(&auth_session),
            Arc::clone(&auth_checker),
            Arc::clone(&auth_grants),
        );
        tracing::info!(
            "ChannelRunner spawned: channel={}, component={} (auth enabled)",
            subagent_channel,
            subagent_id.fqn()
        );

        // Spawn ChannelRunner for shell component (auth verification)
        // Shell uses a non-elevated session so dangerous commands require HIL approval
        let shell_session: Arc<Session> = Arc::new(Session::new(principal.clone()));
        tracing::info!(
            "Shell session created: principal={:?}, elevated={} (HIL approval required for commands)",
            shell_session.principal(),
            shell_session.is_elevated()
        );
        let shell_component = ScriptLoader::load_embedded("shell", Arc::clone(&sandbox))
            .map_err(|e| AppError::Config(format!("Failed to load shell script: {e}")))?;
        let shell_id = shell_component.id().clone();
        let _shell_handle = engine.spawn_runner_full_auth(
            shell_channel,
            Box::new(shell_component),
            Some(io_event_tx.clone()),
            None,
            shell_session,
            Arc::clone(&auth_checker),
            shell_grants,
        );
        tracing::info!(
            "ChannelRunner spawned: channel={}, component={} (auth enabled)",
            shell_channel,
            shell_id.fqn()
        );

        // Spawn ChannelRunner for tool component (file tool verification)
        // Uses auth so ChildContext is set → Capability gating is active
        let tool_grants: Arc<dyn GrantPolicy> = Arc::new(DefaultGrantStore::new());
        let tool_component = ScriptLoader::load_embedded("tool", Arc::clone(&sandbox))
            .map_err(|e| AppError::Config(format!("Failed to load tool script: {e}")))?;
        let tool_id = tool_component.id().clone();
        let _tool_handle = engine.spawn_runner_full_auth(
            tool_channel,
            Box::new(tool_component),
            Some(io_event_tx.clone()),
            None,
            Arc::clone(&auth_session),
            Arc::clone(&auth_checker),
            tool_grants,
        );
        tracing::info!(
            "ChannelRunner spawned: channel={}, component={} (auth enabled)",
            tool_channel,
            tool_id.fqn()
        );

        // Spawn ChannelRunner for agent_mgr with child spawning enabled
        let agent_mgr_component = ScriptLoader::load_embedded("agent_mgr", Arc::clone(&sandbox))
            .map_err(|e| AppError::Config(format!("Failed to load agent_mgr script: {e}")))?;
        let agent_mgr_id = agent_mgr_component.id().clone();
        let lua_loader: Arc<dyn LuaChildLoader> = Arc::new(AppLuaChildLoader {
            sandbox: Arc::clone(&sandbox),
        });
        let _agent_mgr_handle = engine.spawn_runner_full_auth(
            agent_mgr_channel,
            Box::new(agent_mgr_component),
            Some(io_event_tx),
            Some(lua_loader),
            Arc::clone(&auth_session),
            Arc::clone(&auth_checker),
            Arc::clone(&auth_grants),
        );
        tracing::info!(
            "ChannelRunner spawned: channel={}, component={} (child spawner + auth enabled)",
            agent_mgr_channel,
            agent_mgr_id.fqn()
        );

        // Load or create session
        let (session, is_resumed, restored_count) =
            if let Some(session_id) = &self.resume_session_id {
                tracing::info!("Resuming session: {}", session_id);
                let asset = engine.load_session(&store, session_id).await?;
                let count = asset.component_snapshots.len();
                (asset, true, count)
            } else {
                (SessionAsset::new(), false, 0)
            };

        // Create console renderer based on config
        let renderer = if config.ui.verbose {
            ConsoleRenderer::verbose()
        } else {
            ConsoleRenderer::new()
        };

        // Build component routing table (name → channel_id)
        let mut component_routes = HashMap::new();
        component_routes.insert("claude_cli".to_string(), claude_channel);
        component_routes.insert("subagent".to_string(), subagent_channel);
        component_routes.insert("shell".to_string(), shell_channel);
        component_routes.insert("tool".to_string(), tool_channel);
        component_routes.insert("agent_mgr".to_string(), agent_mgr_channel);
        tracing::info!(
            "Component routes registered: {:?}",
            component_routes.keys().collect::<Vec<_>>()
        );

        tracing::debug!("OrcsApp created with config: debug={}", config.debug);

        Ok(OrcsApp {
            config,
            resolver: self.resolver,
            engine,
            principal,
            renderer,
            io_input,
            io_output,
            pending_approval: None,
            store,
            session,
            is_resumed,
            restored_count,
            component_routes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;
    use orcs_runtime::NoOpResolver;
    use std::path::PathBuf;

    fn test_sandbox() -> Arc<dyn orcs_runtime::sandbox::SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    #[tokio::test]
    async fn builder_default() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .unwrap();
        assert!(app.pending_approval_id().is_none());
    }

    #[tokio::test]
    async fn builder_with_custom_resolver() {
        struct VerboseResolver;
        impl ConfigResolver for VerboseResolver {
            fn resolve(&self) -> Result<OrcsConfig, orcs_runtime::ConfigError> {
                let mut config = OrcsConfig::default();
                config.ui.verbose = true;
                Ok(config)
            }
        }

        let app = OrcsApp::builder(VerboseResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .unwrap();
        assert!(app.config().ui.verbose);
    }

    #[tokio::test]
    async fn app_engine_access() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .unwrap();
        assert!(!app.engine().is_running());
    }

    #[tokio::test]
    async fn app_engine_parallel_access() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .unwrap();
        let io = app.engine().io_channel();
        let world = app.engine().world_read();
        let w = world.read().await;
        assert!(w.get(&io).is_some());
    }

    #[tokio::test]
    async fn app_session_id() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .unwrap();
        assert!(!app.session_id().is_empty());
    }

    #[tokio::test]
    async fn app_new_session_not_resumed() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .unwrap();
        assert!(!app.is_resumed());
        assert_eq!(app.restored_count(), 0);
    }

    #[tokio::test]
    async fn app_save_and_load_session() {
        let temp_dir = std::env::temp_dir().join(format!("orcs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        struct SessionResolver {
            session_dir: PathBuf,
        }
        impl ConfigResolver for SessionResolver {
            fn resolve(&self) -> Result<OrcsConfig, orcs_runtime::ConfigError> {
                let mut config = OrcsConfig::default();
                config.paths.session_dir = Some(self.session_dir.clone());
                Ok(config)
            }
        }

        let session_id = {
            let mut app = OrcsApp::builder(SessionResolver {
                session_dir: temp_dir.clone(),
            })
            .with_sandbox(test_sandbox())
            .build()
            .await
            .unwrap();

            let id = app.session_id().to_string();
            app.save_session().await.unwrap();
            id
        };

        let app = OrcsApp::builder(SessionResolver {
            session_dir: temp_dir.clone(),
        })
        .with_sandbox(test_sandbox())
        .resume(&session_id)
        .build()
        .await
        .unwrap();

        assert!(app.is_resumed());
        assert_eq!(app.session_id(), session_id);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn app_resume_nonexistent_session_fails() {
        let temp_dir = std::env::temp_dir().join(format!("orcs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        struct SessionResolver {
            session_dir: PathBuf,
        }
        impl ConfigResolver for SessionResolver {
            fn resolve(&self) -> Result<OrcsConfig, orcs_runtime::ConfigError> {
                let mut config = OrcsConfig::default();
                config.paths.session_dir = Some(self.session_dir.clone());
                Ok(config)
            }
        }

        let result = OrcsApp::builder(SessionResolver {
            session_dir: temp_dir.clone(),
        })
        .with_sandbox(test_sandbox())
        .resume("nonexistent-session-id")
        .build()
        .await;

        assert!(result.is_err());

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn app_reload_config() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ToggleResolver {
            debug: Arc<AtomicBool>,
        }
        impl ConfigResolver for ToggleResolver {
            fn resolve(&self) -> Result<OrcsConfig, orcs_runtime::ConfigError> {
                Ok(OrcsConfig {
                    debug: self.debug.load(Ordering::Relaxed),
                    ..OrcsConfig::default()
                })
            }
        }

        let flag = Arc::new(AtomicBool::new(false));
        let mut app = OrcsApp::builder(ToggleResolver {
            debug: Arc::clone(&flag),
        })
        .with_sandbox(test_sandbox())
        .build()
        .await
        .unwrap();

        assert!(!app.config().debug);

        flag.store(true, Ordering::Relaxed);
        app.reload_config().unwrap();

        assert!(app.config().debug);
    }
}

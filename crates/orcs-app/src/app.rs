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
//! use orcs_app::OrcsApp;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut app = OrcsApp::builder().build().await?;
//!     app.run_interactive().await?;
//!     Ok(())
//! }
//! ```

use crate::{AppError, CliOverrides};
use orcs_component::Component;
use orcs_event::Signal;
use orcs_lua::ScriptLoader;
use orcs_runtime::io::{ConsoleRenderer, InputParser};
use orcs_runtime::{
    ChannelConfig, ConfigLoader, ConfigResolver, IOInput, IOInputHandle, IOOutput, IOOutputHandle,
    IOPort, InputCommand, InputContext, LocalFileStore, OrcsConfig, OrcsEngine, SessionAsset,
    World,
};
use orcs_types::{Principal, PrincipalId};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

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
}

impl OrcsApp {
    /// Creates a new builder for OrcsApp.
    #[must_use]
    pub fn builder() -> OrcsAppBuilder {
        OrcsAppBuilder::new()
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
    /// - **User messages** (Unknown): → IOInput経由でClientRunner → Component
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

    /// Handles an approve command via IOInput.
    ///
    /// Sends to ClientRunner via IOInput with approval ID in context.
    /// ClientRunner will convert to Approve Signal and broadcast.
    fn handle_approve_via_io(&mut self, line: &str, explicit_id: Option<&str>) {
        // Determine approval ID: explicit > pending
        let approval_id = explicit_id
            .map(String::from)
            .or_else(|| self.pending_approval.as_ref().map(|p| p.id.clone()));

        if let Some(id) = approval_id {
            // Build context with approval ID
            let context = InputContext::with_approval_id(&id);
            let io_input = IOInput::line_with_context(line, context);

            match self.io_input.try_send(io_input) {
                Ok(()) => {
                    tracing::debug!("Sent approve via IOInput: {}", id);
                    // Clear pending approval after sending
                    if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(id.as_str()) {
                        self.pending_approval = None;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to send approve via IOInput: {:?}", e);
                    // Fallback: broadcast directly via engine
                    self.handle_approve_fallback(&id);
                }
            }
        } else {
            self.renderer
                .render_output(&IOOutput::warn("No pending approval. Use: y <id>"));
        }
    }

    /// Handles a reject command via IOInput.
    ///
    /// Sends to ClientRunner via IOInput with approval ID in context.
    /// ClientRunner will convert to Reject Signal and broadcast.
    fn handle_reject_via_io(&mut self, line: &str, explicit_id: Option<&str>) {
        // Determine approval ID: explicit > pending
        let approval_id = explicit_id
            .map(String::from)
            .or_else(|| self.pending_approval.as_ref().map(|p| p.id.clone()));

        if let Some(id) = approval_id {
            // Build context with approval ID
            let context = InputContext::with_approval_id(&id);
            let io_input = IOInput::line_with_context(line, context);

            match self.io_input.try_send(io_input) {
                Ok(()) => {
                    tracing::debug!("Sent reject via IOInput: {}", id);
                    // Clear pending approval after sending
                    if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(id.as_str()) {
                        self.pending_approval = None;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to send reject via IOInput: {:?}", e);
                    // Fallback: broadcast directly via engine
                    self.handle_reject_fallback(&id, None);
                }
            }
        } else {
            self.renderer
                .render_output(&IOOutput::warn("No pending approval. Use: n <id> [reason]"));
        }
    }

    /// Fallback: Broadcast approve signal directly via engine.
    ///
    /// Used when IOInput channel is full/closed.
    fn handle_approve_fallback(&mut self, approval_id: &str) {
        let signal = Signal::approve(approval_id, self.principal.clone());
        self.engine.signal(signal);
        self.renderer
            .render_output(&IOOutput::approved(approval_id));
    }

    /// Fallback: Broadcast reject signal directly via engine.
    ///
    /// Used when IOInput channel is full/closed.
    fn handle_reject_fallback(&mut self, approval_id: &str, reason: Option<String>) {
        let signal = Signal::reject(approval_id, reason.clone(), self.principal.clone());
        self.engine.signal(signal);
        self.renderer
            .render_output(&IOOutput::rejected(approval_id, reason));
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
/// # Configuration Loading
///
/// Configuration is loaded from multiple sources with priority:
///
/// 1. CLI overrides (via builder methods like `verbose()`)
/// 2. Environment variables (`ORCS_*`)
/// 3. Project config (`.orcs/config.toml`)
/// 4. Global config (`~/.orcs/config.toml`)
/// 5. Default values
///
/// # Example
///
/// ```ignore
/// let app = OrcsApp::builder()
///     .with_project_root("/path/to/project")
///     .verbose()  // CLI override
///     .build()
///     .await?;
/// ```
pub struct OrcsAppBuilder {
    /// Project root directory for project-local config.
    project_root: Option<PathBuf>,
    /// Session ID to resume from.
    resume_session_id: Option<String>,
    /// CLI overrides (applied after config loading).
    cli_overrides: CliOverrides,
}

impl OrcsAppBuilder {
    /// Creates a new builder with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            project_root: None,
            resume_session_id: None,
            cli_overrides: CliOverrides::new(),
        }
    }

    /// Sets the project root directory.
    ///
    /// Project config will be loaded from `<project_root>/.orcs/config.toml`.
    #[must_use]
    pub fn with_project_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.project_root = Some(path.into());
        self
    }

    /// Enables verbose output (CLI override).
    #[must_use]
    pub fn verbose(mut self) -> Self {
        self.cli_overrides = self.cli_overrides.verbose(true);
        self
    }

    /// Enables debug mode (CLI override).
    #[must_use]
    pub fn debug(mut self) -> Self {
        self.cli_overrides = self.cli_overrides.debug(true);
        self
    }

    /// Sets a custom session storage path (CLI override).
    #[must_use]
    pub fn with_session_path(mut self, path: PathBuf) -> Self {
        self.cli_overrides = self.cli_overrides.session_path(path);
        self
    }

    /// Sets CLI overrides directly.
    #[must_use]
    pub fn with_cli_overrides(mut self, overrides: CliOverrides) -> Self {
        self.cli_overrides = overrides;
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
    /// Loads configuration from all sources, creates the World, Engine,
    /// and spawns the IO channel runner with EchoWithHilComponent.
    ///
    /// # Configuration Priority
    ///
    /// CLI overrides > Environment > Project > Global > Default
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Config`] if configuration loading fails.
    pub async fn build(self) -> Result<OrcsApp, AppError> {
        // Load configuration with hierarchical merging
        let mut loader = ConfigLoader::new();
        if let Some(ref project_root) = self.project_root {
            loader = loader.with_project_root(project_root);
        }

        let mut config = loader.load().map_err(|e| AppError::Config(e.to_string()))?;

        // Apply CLI overrides (highest priority)
        self.cli_overrides.apply(&mut config);

        // Create session store
        let session_path = config.paths.session_dir_or_default();
        let store = LocalFileStore::new(session_path)
            .map_err(|e| AppError::Config(format!("Failed to create session store: {e}")))?;

        // Create World with IO channel and a channel for claude_cli
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());
        let claude_channel = world.create_channel(ChannelConfig::default());

        // Create engine with IO channel (required)
        let mut engine = OrcsEngine::new(world, io);

        // Create IO port for ClientRunner
        let (io_port, io_input, io_output) = IOPort::with_defaults(io);
        let principal = Principal::User(PrincipalId::new());

        // Spawn ClientRunner for IO channel (no component - bridge only)
        let (_io_handle, io_event_tx) = engine.spawn_client_runner(io, io_port, principal.clone());
        tracing::info!("ClientRunner spawned: channel={} (IO bridge)", io);

        // Spawn ChannelRunner for claude_cli with output routed to IO channel
        let lua_component = ScriptLoader::load_embedded("claude_cli")
            .map_err(|e| AppError::Config(format!("Failed to load claude_cli script: {e}")))?;
        let component_id = lua_component.id().clone();
        let _claude_handle = engine.spawn_runner_with_emitter(
            claude_channel,
            Box::new(lua_component),
            Some(io_event_tx),
        );
        tracing::info!(
            "ChannelRunner spawned: channel={}, component={}",
            claude_channel,
            component_id.fqn()
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

        tracing::debug!("OrcsApp created with config: debug={}", config.debug);

        Ok(OrcsApp {
            config,
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
        })
    }
}

impl Default for OrcsAppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builder_default() {
        let app = OrcsApp::builder().build().await.unwrap();
        assert!(app.pending_approval_id().is_none());
    }

    #[tokio::test]
    async fn builder_verbose() {
        let app = OrcsApp::builder().verbose().build().await.unwrap();
        assert!(app.pending_approval_id().is_none());
    }

    #[tokio::test]
    async fn app_engine_access() {
        let app = OrcsApp::builder().build().await.unwrap();
        // Engine is not running until run() is called
        assert!(!app.engine().is_running());
    }

    #[tokio::test]
    async fn app_engine_parallel_access() {
        let app = OrcsApp::builder().build().await.unwrap();
        // WorldManager starts immediately in new(), io_channel is always set
        let io = app.engine().io_channel();
        // Verify the channel exists in World
        let world = app.engine().world_read();
        let w = world.read().await;
        assert!(w.get(&io).is_some());
    }

    #[tokio::test]
    async fn app_session_id() {
        let app = OrcsApp::builder().build().await.unwrap();
        // Session ID is generated at build time
        assert!(!app.session_id().is_empty());
    }

    #[tokio::test]
    async fn app_new_session_not_resumed() {
        let app = OrcsApp::builder().build().await.unwrap();
        // New session is not resumed
        assert!(!app.is_resumed());
        assert_eq!(app.restored_count(), 0);
    }

    #[tokio::test]
    async fn app_save_and_load_session() {
        // Use a unique temp directory for this test
        let temp_dir = std::env::temp_dir().join(format!("orcs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create and save a session
        let session_id = {
            let mut app = OrcsApp::builder()
                .with_session_path(temp_dir.clone())
                .build()
                .await
                .unwrap();

            let id = app.session_id().to_string();
            app.save_session().await.unwrap();
            id
        };

        // Resume the session
        let app = OrcsApp::builder()
            .with_session_path(temp_dir.clone())
            .resume(&session_id)
            .build()
            .await
            .unwrap();

        // Verify resumed state
        assert!(app.is_resumed());
        assert_eq!(app.session_id(), session_id);

        // Cleanup
        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn app_resume_nonexistent_session_fails() {
        let temp_dir = std::env::temp_dir().join(format!("orcs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let result = OrcsApp::builder()
            .with_session_path(temp_dir.clone())
            .resume("nonexistent-session-id")
            .build()
            .await;

        assert!(result.is_err());

        // Cleanup
        std::fs::remove_dir_all(&temp_dir).ok();
    }
}

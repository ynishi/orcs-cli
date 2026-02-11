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

mod builder;

pub use builder::OrcsAppBuilder;

use crate::AppError;
use orcs_event::Signal;
use orcs_runtime::auth::{DefaultGrantStore, GrantPolicy};
use orcs_runtime::io::{ConsoleRenderer, InputParser};
use orcs_runtime::session::SessionStore;
use orcs_runtime::{
    ConfigResolver, IOInput, IOInputHandle, IOOutput, IOOutputHandle, InputCommand, InputContext,
    LocalFileStore, OrcsConfig, OrcsEngine, SessionAsset,
};
use orcs_types::{ChannelId, Principal};
use std::collections::HashMap;
use std::sync::Arc;
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
    pub(super) config: OrcsConfig,
    /// Configuration resolver (retained for hot-reload).
    pub(super) resolver: Box<dyn ConfigResolver>,
    pub(super) engine: OrcsEngine,
    /// Principal for signal creation.
    pub(super) principal: Principal,
    /// Console renderer for output display.
    pub(super) renderer: ConsoleRenderer,
    /// Handle for sending input to ClientRunner via IOPort.
    ///
    /// Primary input channel for View → ClientRunner communication.
    /// All user input flows through this channel.
    pub(super) io_input: IOInputHandle,
    /// Handle for receiving IO output from ClientRunner.
    pub(super) io_output: IOOutputHandle,
    /// Pending approval (for "y" / "n" without explicit ID).
    pending_approval: Option<PendingApproval>,
    /// Session store for persistence.
    pub(super) store: LocalFileStore,
    /// Current session asset.
    pub(super) session: SessionAsset,
    /// Whether this session was resumed from storage.
    pub(super) is_resumed: bool,
    /// Number of components restored (if resumed).
    pub(super) restored_count: usize,
    /// Shared grant store for elevated components.
    ///
    /// Retained so that `save_session` can snapshot grants and
    /// `resume` can restore them. Concrete type allows direct
    /// `restore_grants` call without downcasting.
    pub(super) shared_grants: Arc<DefaultGrantStore>,
    /// Component name → ChannelId routing table.
    ///
    /// Used by `@component message` syntax for targeted delivery.
    pub(super) component_routes: HashMap<String, ChannelId>,
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
    /// Transfers component snapshots from the engine into the session
    /// asset, then persists to the store.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Storage`] if saving fails.
    pub async fn save_session(&mut self) -> Result<(), AppError> {
        // Save component snapshots
        for (fqn, snapshot) in self.engine.collected_snapshots() {
            self.session
                .component_snapshots
                .insert(fqn.clone(), snapshot.clone());
        }
        // Save grant state
        match self.shared_grants.list_grants() {
            Ok(grants) => {
                tracing::debug!("Saving {} grant(s) to session", grants.len());
                self.session.save_grants(grants);
            }
            Err(e) => {
                tracing::error!("Failed to list grants for session save: {e}");
            }
        }
        self.session.touch();
        self.store.save(&self.session).await?;
        Ok(())
    }

    /// Loads a session from storage.
    ///
    /// # Arguments
    ///
    /// * `session_id` - The session ID to load
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Storage`] if loading fails.
    pub async fn load_session(&mut self, session_id: &str) -> Result<(), AppError> {
        self.session = self.store.load(session_id).await?;
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
            .expect("builder should succeed with NoOpResolver");
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
            .expect("builder should succeed with VerboseResolver");
        assert!(app.config().ui.verbose);
    }

    #[tokio::test]
    async fn app_engine_access() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .expect("builder should succeed");
        assert!(!app.engine().is_running());
    }

    #[tokio::test]
    async fn app_engine_parallel_access() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .expect("builder should succeed");
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
            .expect("builder should succeed");
        assert!(!app.session_id().is_empty());
    }

    #[tokio::test]
    async fn app_new_session_not_resumed() {
        let app = OrcsApp::builder(NoOpResolver)
            .with_sandbox(test_sandbox())
            .build()
            .await
            .expect("builder should succeed");
        assert!(!app.is_resumed());
        assert_eq!(app.restored_count(), 0);
    }

    #[tokio::test]
    async fn app_save_and_load_session() {
        let temp_dir = std::env::temp_dir().join(format!("orcs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir for session test");

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
            .expect("builder should succeed with SessionResolver");

            let id = app.session_id().to_string();
            app.save_session()
                .await
                .expect("save session should succeed");
            id
        };

        let app = OrcsApp::builder(SessionResolver {
            session_dir: temp_dir.clone(),
        })
        .with_sandbox(test_sandbox())
        .resume(&session_id)
        .build()
        .await
        .expect("resume should succeed for saved session");

        assert!(app.is_resumed());
        assert_eq!(app.session_id(), session_id);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn app_resume_nonexistent_session_fails() {
        let temp_dir = std::env::temp_dir().join(format!("orcs-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

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
        .expect("builder should succeed with ToggleResolver");

        assert!(!app.config().debug);

        flag.store(true, Ordering::Relaxed);
        app.reload_config().expect("reload config should succeed");

        assert!(app.config().debug);
    }
}

//! ORCS Application.
//!
//! High-level application wrapper that integrates:
//!
//! - [`OrcsEngine`] - Core execution engine
//! - [`EchoWithHilComponent`] - Echo with HIL integration (via ChannelRunner)
//! - [`HumanInput`] - stdin command parsing
//! - [`OutputSink`] - User output display
//!
//! # Flow
//!
//! ```text
//! User Input → Event injection → ChannelRunner → EchoWithHilComponent
//!           → HIL承認要求 → Y/N待ち → Echo出力
//! ```
//!
//! # Architecture
//!
//! The EchoWithHilComponent runs inside ChannelRunner (parallel tokio task).
//! App manages approval state locally - Component communicates via Event/Signal only.
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

use crate::AppError;
use orcs_event::{EventCategory, Signal};
use orcs_runtime::{
    default_session_path, ChannelConfig, ChannelHandle, ConsoleOutput, EchoWithHilComponent, Event,
    HumanInput, InputCommand, LocalFileStore, OrcsEngine, OutputSink, SessionAsset, World,
};
use orcs_types::ComponentId;
use std::path::PathBuf;
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
    engine: OrcsEngine,
    input: HumanInput,
    output: Arc<dyn OutputSink>,
    /// Handle for injecting events into the IO channel.
    ///
    /// This is always available after construction (not Option).
    io_handle: ChannelHandle,
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

    /// Returns the output sink.
    #[must_use]
    pub fn output(&self) -> &Arc<dyn OutputSink> {
        &self.output
    }

    /// Returns the IO channel handle for event injection.
    ///
    /// The IO handle is always available after construction.
    #[must_use]
    pub fn io_handle(&self) -> &ChannelHandle {
        &self.io_handle
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
        self.output
            .info("Interactive mode started. Type 'q' to quit, 'help' for commands.");

        // Display session status with restore information
        if self.is_resumed {
            self.output.info(&format!(
                "Resumed session: {} ({} component(s) restored)",
                self.session.id, self.restored_count
            ));
        } else {
            self.output
                .info(&format!("Session ID: {}", self.session.id));
        }

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        let mut should_save = false;

        loop {
            // Poll engine once
            if !self.engine.is_running() {
                break;
            }

            // Check for stdin input with timeout
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
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                    // Yield to allow engine polling
                }
            }

            tokio::task::yield_now().await;
        }

        self.engine.stop();

        // Save session if paused
        if should_save {
            self.output.info("Saving session...");
            self.save_session().await?;
            self.output
                .info(&format!("Session saved: {}", self.session.id));
            self.output
                .info(&format!("Resume with: orcs --resume {}", self.session.id));
        }

        self.output.info("Shutting down...");
        Ok(())
    }

    /// Handles a single line of input.
    ///
    /// Returns control flow instruction for the main loop.
    fn handle_input(&mut self, line: &str) -> LoopControl {
        let cmd = self.input.parse_line(line);

        match &cmd {
            InputCommand::Quit => {
                tracing::info!("Quit requested");
                return LoopControl::Exit;
            }
            InputCommand::Veto => {
                tracing::warn!("Veto signal sent");
                let signal = Signal::veto(self.input.principal().clone());
                // Broadcast to all runners (including EchoWithHilComponent)
                self.engine.signal(signal);
                return LoopControl::Exit;
            }
            InputCommand::Approve { approval_id } => {
                self.handle_approve(approval_id.as_deref());
            }
            InputCommand::Reject {
                approval_id,
                reason,
            } => {
                self.handle_reject(approval_id.as_deref(), reason.clone());
            }
            InputCommand::Pause => {
                tracing::info!("Pause requested");
                return LoopControl::Pause;
            }
            InputCommand::Resume => {
                // Resume is handled at startup via --resume flag, not during interactive mode
                self.output
                    .info("Resume is handled at startup. Use: orcs --resume");
            }
            InputCommand::Steer { message } => {
                self.output.info(&format!("Steer: {}", message));
                // TODO: Implement steer
            }
            InputCommand::Empty => {
                // Blank line - ignore
            }
            InputCommand::Unknown { input } => {
                if input == "help" {
                    self.show_help();
                } else if !input.is_empty() {
                    // Send to Echo component via Event injection
                    self.handle_echo_input(input);
                }
            }
        }

        LoopControl::Continue
    }

    /// Handles user input by injecting an Event to the ChannelRunner.
    ///
    /// App generates the approval_id and includes it in the Event payload.
    /// This allows App to track pending approvals without accessing Component state.
    fn handle_echo_input(&mut self, input: &str) {
        // Generate approval_id in App (not Component)
        let approval_id = uuid::Uuid::new_v4().to_string();

        // Create event with approval_id in payload
        let event = Event {
            category: EventCategory::Echo,
            operation: "echo".to_string(),
            source: ComponentId::builtin("app"),
            payload: serde_json::json!({
                "message": input,
                "approval_id": approval_id
            }),
        };

        // Try to inject event (non-blocking)
        match self.io_handle.try_inject(event) {
            Ok(()) => {
                // Store for later use with "y" command
                self.pending_approval = Some(PendingApproval {
                    id: approval_id.clone(),
                });

                self.output.info(&format!(
                    "Awaiting approval for: '{}' [id: {}]",
                    input, approval_id
                ));
                self.output.info("Type 'y' to approve or 'n' to reject");
            }
            Err(e) => {
                self.output
                    .error(&format!("Failed to inject event: {:?}", e));
            }
        }
    }

    /// Handles an approve command.
    ///
    /// Broadcasts an Approve signal via the engine.
    /// Uses pending approval if no explicit ID provided.
    fn handle_approve(&mut self, approval_id: Option<&str>) {
        let id = approval_id
            .map(String::from)
            .or_else(|| self.pending_approval.as_ref().map(|p| p.id.clone()));

        if let Some(id) = id {
            let signal = Signal::approve(&id, self.input.principal().clone());
            // Broadcast to all runners (ChannelRunner delivers to Component)
            self.engine.signal(signal);
            self.output.show_approved(&id);

            // Clear pending approval after use
            if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(&id) {
                self.pending_approval = None;
            }
        } else {
            self.output.warn("No pending approval. Use: y <id>");
        }
    }

    /// Handles a reject command.
    ///
    /// Broadcasts a Reject signal via the engine.
    /// Uses pending approval if no explicit ID provided.
    fn handle_reject(&mut self, approval_id: Option<&str>, reason: Option<String>) {
        let id = approval_id
            .map(String::from)
            .or_else(|| self.pending_approval.as_ref().map(|p| p.id.clone()));

        if let Some(id) = id {
            let signal = Signal::reject(&id, reason.clone(), self.input.principal().clone());
            // Broadcast to all runners (ChannelRunner delivers to Component)
            self.engine.signal(signal);
            self.output.show_rejected(&id, reason.as_deref());

            // Clear pending approval after use
            if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(&id) {
                self.pending_approval = None;
            }
        } else {
            self.output
                .warn("No pending approval. Use: n <id> [reason]");
        }
    }

    /// Shows help text.
    fn show_help(&self) {
        self.output.info("Commands:");
        self.output
            .info("  y [id]         - Approve pending request");
        self.output
            .info("  n [id] [reason]- Reject pending request");
        self.output.info("  p / pause      - Pause execution");
        self.output.info("  r / resume     - Resume execution");
        self.output
            .info("  s <message>    - Steer with instruction");
        self.output.info("  q / quit       - Quit application");
        self.output.info("  veto / stop    - Emergency stop");
    }
}

/// Builder for [`OrcsApp`].
pub struct OrcsAppBuilder {
    output: Option<Arc<dyn OutputSink>>,
    verbose: bool,
    session_path: Option<PathBuf>,
    resume_session_id: Option<String>,
}

impl OrcsAppBuilder {
    /// Creates a new builder with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            output: None,
            verbose: false,
            session_path: None,
            resume_session_id: None,
        }
    }

    /// Sets a custom output sink.
    #[must_use]
    pub fn with_output(mut self, output: Arc<dyn OutputSink>) -> Self {
        self.output = Some(output);
        self
    }

    /// Enables verbose output.
    #[must_use]
    pub fn verbose(mut self) -> Self {
        self.verbose = true;
        self
    }

    /// Sets a custom session storage path.
    #[must_use]
    pub fn with_session_path(mut self, path: PathBuf) -> Self {
        self.session_path = Some(path);
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
    /// Creates the World, Engine, and spawns the IO channel runner with
    /// EchoWithHilComponent. All components are initialized at build time.
    ///
    /// If `resume` was called, loads the specified session and restores
    /// component state.
    pub async fn build(self) -> Result<OrcsApp, AppError> {
        // Create session store
        let session_path = self.session_path.unwrap_or_else(default_session_path);
        let store = LocalFileStore::new(session_path)
            .map_err(|e| AppError::Config(format!("Failed to create session store: {}", e)))?;

        // Create World with IO channel
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());

        // Create engine with IO channel (required)
        let mut engine = OrcsEngine::new(world, io);

        // Spawn runner for IO channel with EchoWithHilComponent
        let echo_component = Box::new(EchoWithHilComponent::new());
        let io_handle = engine.spawn_runner(io, echo_component);
        tracing::info!(
            "IO channel runner spawned with EchoWithHilComponent: {}",
            io
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

        // Create output sink
        let output: Arc<dyn OutputSink> = self.output.unwrap_or_else(|| {
            if self.verbose {
                Arc::new(ConsoleOutput::verbose())
            } else {
                Arc::new(ConsoleOutput::new())
            }
        });

        // Create input handler
        let input = HumanInput::new();

        tracing::debug!("OrcsApp created (fully initialized)");

        Ok(OrcsApp {
            engine,
            input,
            output,
            io_handle,
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
    async fn app_io_handle_available() {
        let app = OrcsApp::builder().build().await.unwrap();
        // io_handle is set at build time (not Option)
        let io_channel = app.engine().io_channel();
        assert_eq!(app.io_handle().id, io_channel);
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

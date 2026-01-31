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
//!     let mut app = OrcsApp::builder().build()?;
//!     app.run_interactive().await?;
//!     Ok(())
//! }
//! ```

use crate::AppError;
use orcs_event::{EventCategory, Signal};
use orcs_runtime::{
    ChannelConfig, ChannelHandle, ConsoleOutput, EchoWithHilComponent, Event, HumanInput,
    InputCommand, OrcsEngine, OutputSink, World,
};
use orcs_types::ComponentId;
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

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();

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
                            if !self.handle_input(&line) {
                                break;
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
        self.output.info("Shutting down...");
        Ok(())
    }

    /// Handles a single line of input.
    ///
    /// Returns `false` if the application should quit.
    fn handle_input(&mut self, line: &str) -> bool {
        let cmd = self.input.parse_line(line);

        match &cmd {
            InputCommand::Quit => {
                tracing::info!("Quit requested");
                return false;
            }
            InputCommand::Veto => {
                tracing::warn!("Veto signal sent");
                let signal = Signal::veto(self.input.principal().clone());
                // Broadcast to all runners (including EchoWithHilComponent)
                self.engine.signal(signal);
                return false;
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
                self.output
                    .info("Pause not yet implemented for interactive mode");
            }
            InputCommand::Resume => {
                self.output
                    .info("Resume not yet implemented for interactive mode");
            }
            InputCommand::Steer { message } => {
                self.output.info(&format!("Steer: {}", message));
                // TODO: Implement steer
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

        true
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
}

impl OrcsAppBuilder {
    /// Creates a new builder with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            output: None,
            verbose: false,
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

    /// Builds the application.
    ///
    /// Creates the World, Engine, and spawns the IO channel runner with
    /// EchoWithHilComponent. All components are initialized at build time.
    pub fn build(self) -> Result<OrcsApp, AppError> {
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
        let app = OrcsApp::builder().build().unwrap();
        assert!(app.pending_approval_id().is_none());
    }

    #[tokio::test]
    async fn builder_verbose() {
        let app = OrcsApp::builder().verbose().build().unwrap();
        assert!(app.pending_approval_id().is_none());
    }

    #[tokio::test]
    async fn app_engine_access() {
        let app = OrcsApp::builder().build().unwrap();
        // Engine is not running until run() is called
        assert!(!app.engine().is_running());
    }

    #[tokio::test]
    async fn app_engine_parallel_access() {
        let app = OrcsApp::builder().build().unwrap();
        // WorldManager starts immediately in new(), io_channel is always set
        let io = app.engine().io_channel();
        // Verify the channel exists in World
        let world = app.engine().world_read();
        let w = world.read().await;
        assert!(w.get(&io).is_some());
    }

    #[tokio::test]
    async fn app_io_handle_available() {
        let app = OrcsApp::builder().build().unwrap();
        // io_handle is set at build time (not Option)
        let io_channel = app.engine().io_channel();
        assert_eq!(app.io_handle().id, io_channel);
    }
}

//! ORCS Application.
//!
//! High-level application wrapper that integrates:
//!
//! - [`OrcsEngine`] - Core execution engine
//! - [`EchoWithHilComponent`] - Echo with HIL integration (front-facing)
//! - [`HumanInput`] - stdin command parsing
//! - [`OutputSink`] - User output display
//!
//! # Flow
//!
//! ```text
//! User Input → EchoWithHilComponent → HIL承認要求 → Y/N待ち → Echo出力
//! ```
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
use orcs_component::Component;
use orcs_event::{EventCategory, Request, Signal};
use orcs_runtime::{
    ConsoleOutput, EchoWithHilComponent, HumanInput, InputCommand, OrcsEngine, OutputSink, World,
};
use orcs_types::ComponentId;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

/// ORCS Application.
///
/// Integrates the engine with HIL, input handling, and output display.
pub struct OrcsApp {
    engine: OrcsEngine,
    input: HumanInput,
    output: Arc<dyn OutputSink>,
    /// Echo component with HIL integration (front-facing for user input).
    echo: EchoWithHilComponent,
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

    /// Returns a reference to the echo component.
    #[must_use]
    pub fn echo(&self) -> &EchoWithHilComponent {
        &self.echo
    }

    /// Runs the application in interactive mode.
    ///
    /// Reads commands from stdin and processes them until quit.
    pub async fn run_interactive(&mut self) -> Result<(), AppError> {
        tracing::info!("Starting interactive mode");
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
                self.echo.on_signal(&signal);
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
                    // Send to Echo component for HIL processing
                    self.handle_echo_input(input);
                }
            }
        }

        true
    }

    /// Handles user input by sending it to the Echo component.
    ///
    /// The Echo component will request HIL approval before echoing.
    fn handle_echo_input(&mut self, input: &str) {
        let channel_id = match self.engine.world().primary() {
            Some(id) => id,
            None => {
                self.output
                    .error("No primary channel. Application not properly initialized.");
                return;
            }
        };

        let request = Request::new(
            EventCategory::Echo,
            "echo",
            ComponentId::builtin("app"),
            channel_id,
            Value::String(input.to_string()),
        );

        match self.echo.on_request(&request) {
            Ok(response) => {
                if let Some(approval_id) = response.get("approval_id").and_then(|v| v.as_str()) {
                    self.output.info(&format!(
                        "Awaiting approval for: '{}' [approval_id: {}]",
                        input, approval_id
                    ));
                    self.output.info("Type 'y' to approve or 'n' to reject");
                }
            }
            Err(e) => {
                self.output.error(&format!("Echo error: {}", e));
            }
        }
    }

    /// Handles an approve command.
    fn handle_approve(&mut self, approval_id: Option<&str>) {
        // Use pending approval ID if not specified (clone to avoid borrow issues)
        let id: Option<String> = approval_id
            .map(String::from)
            .or_else(|| self.echo.pending_approval_id().map(String::from));

        if let Some(id) = id {
            let signal = Signal::approve(&id, self.input.principal().clone());
            // Send to Echo component (which has internal HIL)
            self.echo.on_signal(&signal);
            self.output.show_approved(&id);

            // Check if echo produced a result
            if let Some(result) = self.echo.last_result() {
                self.output.info(result);
            }
        } else {
            self.output.warn("No pending approval. Use: y <id>");
        }
    }

    /// Handles a reject command.
    fn handle_reject(&mut self, approval_id: Option<&str>, reason: Option<String>) {
        // Use pending approval ID if not specified (clone to avoid borrow issues)
        let id: Option<String> = approval_id
            .map(String::from)
            .or_else(|| self.echo.pending_approval_id().map(String::from));

        if let Some(id) = id {
            let signal = Signal::reject(&id, reason.clone(), self.input.principal().clone());
            // Send to Echo component (which has internal HIL)
            self.echo.on_signal(&signal);
            self.output.show_rejected(&id, reason.as_deref());

            // Check if echo produced a result (rejection message)
            if let Some(result) = self.echo.last_result() {
                self.output.info(result);
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
    pub fn build(self) -> Result<OrcsApp, AppError> {
        // Create World with primary channel
        let mut world = World::new();
        world.create_primary()?;

        // Create engine
        let engine = OrcsEngine::new(world);

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

        // Create Echo component with HIL integration
        let echo = EchoWithHilComponent::new();
        tracing::debug!("EchoWithHilComponent created");

        Ok(OrcsApp {
            engine,
            input,
            output,
            echo,
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

    #[test]
    fn builder_default() {
        let app = OrcsApp::builder().build().unwrap();
        // Echo component should be created
        assert!(!app.echo().has_pending());
    }

    #[test]
    fn builder_verbose() {
        let app = OrcsApp::builder().verbose().build().unwrap();
        assert!(!app.echo().has_pending());
    }

    #[test]
    fn app_engine_access() {
        let app = OrcsApp::builder().build().unwrap();
        // Engine is not running until run() is called
        assert!(!app.engine().is_running());
        // But we can access the world
        assert!(app.engine().world().primary().is_some());
    }

    #[test]
    fn app_echo_input_creates_pending() {
        let mut app = OrcsApp::builder().build().unwrap();

        // Simulate user input
        let channel_id = app.engine().world().primary().unwrap();
        let request = Request::new(
            EventCategory::Echo,
            "echo",
            ComponentId::builtin("test"),
            channel_id,
            Value::String("hello".into()),
        );

        let result = app.echo.on_request(&request);
        assert!(result.is_ok());
        assert!(app.echo.has_pending());
    }
}

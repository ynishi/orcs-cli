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
use crate::SharedPrinterSlot;
use orcs_event::Signal;
use orcs_runtime::auth::{DefaultGrantStore, GrantPolicy};
use orcs_runtime::io::{ConsoleRenderer, InputParser};
use orcs_runtime::session::SessionStore;
use orcs_runtime::{
    ConfigResolver, IOInput, IOInputHandle, IOOutput, IOOutputHandle, InputCommand, InputContext,
    LocalFileStore, OrcsConfig, OrcsEngine, SessionAsset,
};
use orcs_types::{ChannelId, Principal, PrincipalId};
use rustyline::ExternalPrinter as RustylineExternalPrinter;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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

/// Event sent from the dedicated readline OS thread to the async main loop.
#[derive(Debug)]
enum ReadlineEvent {
    /// User entered a line of text.
    Line(String),
    /// EOF (Ctrl+D on empty line).
    Eof,
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
    /// Shared printer slot for terminal-safe output while rustyline is active.
    ///
    /// Shared with the tracing `MakeWriter` so that log output also routes
    /// through ExternalPrinter during interactive mode.
    printer_slot: SharedPrinterSlot,
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

    /// Returns the history file path from config, or `~/.orcs/history` as default.
    fn history_path(&self) -> PathBuf {
        self.config.paths.history_file_or_default()
    }

    /// Spawns a dedicated OS thread running rustyline for line editing.
    ///
    /// Returns:
    /// - `UnboundedReceiver<ReadlineEvent>` for the async loop to consume
    /// - `Option<Box<dyn ExternalPrinter>>` for terminal-safe output during readline
    ///
    /// The thread saves history to disk after each entered line.
    fn spawn_readline_thread(
        history_path: PathBuf,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<ReadlineEvent>,
        Option<Box<dyn RustylineExternalPrinter + Send>>,
    ) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (printer_tx, printer_rx) =
            std::sync::mpsc::sync_channel::<Option<Box<dyn RustylineExternalPrinter + Send>>>(1);

        if let Err(e) = std::thread::Builder::new()
            .name("orcs-readline".into())
            .spawn(move || {
                let config = rustyline::Config::builder().auto_add_history(true).build();

                let mut rl = match rustyline::DefaultEditor::with_config(config) {
                    Ok(editor) => editor,
                    Err(e) => {
                        tracing::error!("Failed to create readline editor: {e}");
                        let _ = printer_tx.send(None);
                        return;
                    }
                };

                // Load history (ignore errors for first run)
                if let Err(e) = rl.load_history(&history_path) {
                    tracing::debug!("History load: {e} (expected on first run)");
                }

                // Create ExternalPrinter for async-side output during readline
                let printer = rl
                    .create_external_printer()
                    .ok()
                    .map(|p| Box::new(p) as Box<dyn RustylineExternalPrinter + Send>);
                let _ = printer_tx.send(printer);

                // Readline loop
                loop {
                    match rl.readline("orcs> ") {
                        Ok(line) => {
                            // Persist history after each line (resilient to process kill)
                            let _ = rl.save_history(&history_path);

                            if event_tx.send(ReadlineEvent::Line(line)).is_err() {
                                break; // Receiver dropped → shutdown
                            }
                        }
                        Err(rustyline::error::ReadlineError::Interrupted) => {
                            // Ctrl+C → clear current line, continue
                            continue;
                        }
                        Err(rustyline::error::ReadlineError::Eof) => {
                            let _ = event_tx.send(ReadlineEvent::Eof);
                            break;
                        }
                        Err(e) => {
                            tracing::error!("Readline error: {e}");
                            let _ = event_tx.send(ReadlineEvent::Eof);
                            break;
                        }
                    }
                }

                // Final history save
                let _ = rl.save_history(&history_path);
            })
        {
            tracing::error!("failed to spawn readline thread: {e}");
        }

        // Block until the readline thread sends the printer (or fails)
        let printer = printer_rx.recv().ok().flatten();

        (event_rx, printer)
    }

    /// Renders output through ExternalPrinter (if active) or ConsoleRenderer.
    ///
    /// During `run_interactive()`, all output must go through ExternalPrinter
    /// to avoid corrupting rustyline's terminal state (raw mode, prompt redraw).
    fn render_safe(&self, output: &IOOutput) {
        if let Some(formatted) = Self::format_io_output(output, self.renderer.is_verbose()) {
            if self.printer_slot.print(formatted) {
                return;
            }
        }
        // Fallback: direct console output (no printer installed)
        self.renderer.render_output(output);
    }

    /// Formats an IOOutput into a terminal-ready string for ExternalPrinter.
    ///
    /// Returns `None` for messages that should be suppressed (e.g. Debug when not verbose).
    fn format_io_output(output: &IOOutput, verbose: bool) -> Option<String> {
        match output {
            IOOutput::Print { text, style } => match style {
                orcs_runtime::OutputStyle::Normal | orcs_runtime::OutputStyle::Info => {
                    Some(format!("{text}\n"))
                }
                orcs_runtime::OutputStyle::Warn => Some(format!("[WARN] {text}\n")),
                orcs_runtime::OutputStyle::Error => Some(format!("[ERROR] {text}\n")),
                orcs_runtime::OutputStyle::Success => Some(format!("\x1B[32m{text}\x1B[0m\n")),
                orcs_runtime::OutputStyle::Debug => {
                    if verbose {
                        Some(format!("[DEBUG] {text}\n"))
                    } else {
                        None
                    }
                }
            },
            IOOutput::Prompt { message } => Some(format!("{message} ")),
            IOOutput::ShowApprovalRequest {
                id,
                operation,
                description,
            } => Some(format!(
                "\n  [{operation}] {id} - {description}\n  Enter 'y' to approve, 'n' to reject:\n"
            )),
            IOOutput::ShowApproved { approval_id } => {
                Some(format!("  \u{2713} Approved: {approval_id}\n"))
            }
            IOOutput::ShowRejected {
                approval_id,
                reason,
            } => {
                if let Some(reason) = reason {
                    Some(format!("  \u{2717} Rejected: {approval_id} ({reason})\n"))
                } else {
                    Some(format!("  \u{2717} Rejected: {approval_id}\n"))
                }
            }
            IOOutput::ShowProcessing {
                component,
                operation,
            } => Some(format!("  [{component}] Processing ({operation})...\n")),
            IOOutput::Clear => Some("\x1B[2J\x1B[1;1H".to_string()),
        }
    }

    /// Runs the application in non-interactive command mode.
    ///
    /// Sends a single command, collects output until idle, and exits.
    /// Unlike interactive mode, raw text is sent directly without
    /// interactive command parsing (`q`/`y`/`n` are sent as-is).
    ///
    /// # Output routing
    ///
    /// - `Normal`/`Info`/`Success`/`Debug` → stdout
    /// - `Warn`/`Error` → stderr
    ///
    /// # Returns
    ///
    /// Exit code: 0 for success, 1 if errors occurred.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Io`] on communication errors.
    pub async fn run_command(&mut self, command: &str) -> Result<i32, AppError> {
        self.engine.start();

        let trimmed = command.trim();
        if trimmed.is_empty() {
            self.engine.stop();
            self.engine.shutdown().await;
            return Ok(0);
        }

        // Route: @component messages use targeted injection,
        // everything else goes through IOInput as user message.
        if trimmed.starts_with('@') {
            let cmd = InputParser.parse(trimmed);
            match cmd {
                InputCommand::ComponentMessage {
                    ref target,
                    ref message,
                } => {
                    self.handle_component_message(target, message);
                }
                _ => {
                    // Bare "@" or invalid target
                    eprintln!("Invalid component target: {trimmed}");
                    self.engine.stop();
                    self.engine.shutdown().await;
                    return Ok(1);
                }
            }
        } else {
            // Send raw text directly (no interactive command parsing).
            let io_input = IOInput::line(command);
            if let Err(e) = self.io_input.try_send(io_input) {
                eprintln!("Failed to send command: {e:?}");
                self.engine.stop();
                self.engine.shutdown().await;
                return Ok(1);
            }
        }

        // Collect output with idle-timeout detection.
        //   - Before first output: wait up to MAX_WAIT (LLM may be slow to start).
        //   - After first output:  wait up to IDLE for more (stream complete).
        const IDLE: tokio::time::Duration = tokio::time::Duration::from_secs(3);
        const MAX_WAIT: tokio::time::Duration = tokio::time::Duration::from_secs(120);

        let start = tokio::time::Instant::now();
        let mut received_output = false;
        let mut exit_code: i32 = 0;

        loop {
            if start.elapsed() > MAX_WAIT {
                tracing::warn!(
                    "Command mode: max timeout reached ({}s)",
                    MAX_WAIT.as_secs()
                );
                break;
            }

            let timeout = if received_output { IDLE } else { MAX_WAIT };

            tokio::select! {
                Some(io_out) = self.io_output.recv() => {
                    match &io_out {
                        IOOutput::Print { text, style } => {
                            received_output = true;
                            if style.is_warning_or_error() {
                                exit_code = 1;
                                eprintln!("{text}");
                            } else {
                                println!("{text}");
                            }
                        }
                        IOOutput::Prompt { .. } => {
                            // System ready for next input → command complete.
                            break;
                        }
                        IOOutput::ShowApprovalRequest { .. } => {
                            // Non-interactive mode cannot handle HIL approval.
                            eprintln!("Command requires interactive approval. Use interactive mode.");
                            exit_code = 1;
                            break;
                        }
                        _ => {}
                    }
                }
                () = tokio::time::sleep(timeout) => {
                    tracing::debug!(
                        "Command mode: timeout (received_output={received_output})",
                    );
                    break;
                }
            }
        }

        self.engine.stop();
        self.engine.shutdown().await;
        Ok(exit_code)
    }

    /// Runs the application in interactive mode.
    ///
    /// Uses a dedicated OS thread for rustyline (prompt, history, line editing)
    /// and communicates with the async main loop via channels.
    ///
    /// All terminal output during the loop goes through `ExternalPrinter`
    /// to avoid corrupting rustyline's raw-mode terminal state.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Io`] on stdin read errors.
    pub async fn run_interactive(&mut self) -> Result<(), AppError> {
        let io_id = self.engine.io_channel();
        tracing::info!("Starting interactive mode (IO channel: {})", io_id);

        self.engine.start();

        // Startup messages (before readline, ext_printer is None → ConsoleRenderer)
        self.renderer.render_output(&IOOutput::info(
            "Interactive mode started. Type 'q' to quit, 'help' for commands.",
        ));
        if self.is_resumed {
            self.renderer.render_output(&IOOutput::info(format!(
                "Resumed session: {} ({} component(s) restored)",
                self.session.id, self.restored_count
            )));
        } else {
            self.renderer
                .render_output(&IOOutput::info(format!("Session ID: {}", self.session.id)));
        }

        // Spawn readline thread and install ExternalPrinter into shared slot
        // (tracing MakeWriter also reads this slot)
        let (mut readline_rx, printer) = Self::spawn_readline_thread(self.history_path());
        if let Some(p) = printer {
            self.printer_slot.set(p);
        }

        let mut should_save = false;

        // Yield to let pending component init messages (Ready, etc.) be delivered.
        // Without this, piped stdin can race ahead of async IOOutput.
        tokio::task::yield_now().await;

        loop {
            if !self.engine.is_running() {
                break;
            }

            // biased: always drain IOOutput before processing stdin.
            // This ensures component messages (Ready, approval requests, etc.)
            // are displayed before the next input line is handled.
            tokio::select! {
                biased;

                Some(io_out) = self.io_output.recv() => {
                    self.handle_io_output(io_out);
                }
                event = readline_rx.recv() => {
                    match event {
                        Some(ReadlineEvent::Line(line)) => {
                            match self.handle_input(&line) {
                                LoopControl::Continue => {}
                                LoopControl::Exit => break,
                                LoopControl::Pause => {
                                    should_save = true;
                                    break;
                                }
                            }
                        }
                        Some(ReadlineEvent::Eof) | None => {
                            tracing::debug!("readline: EOF or channel closed");
                            break;
                        }
                    }
                }
            }
        }

        // Clear ExternalPrinter before shutdown output
        // (tracing falls back to stderr, ConsoleRenderer writes directly)
        self.printer_slot.clear();

        self.engine.stop();
        self.engine.shutdown().await;

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
                self.render_safe(&IOOutput::info(
                    "Resume is handled at startup. Use: orcs --resume",
                ));
            }
            InputCommand::Steer { message } => {
                let io_id = self.engine.io_channel();
                self.engine.signal(Signal::steer(
                    io_id,
                    message.as_str(),
                    Principal::User(PrincipalId::new()),
                ));
                self.render_safe(&IOOutput::info(format!("Steer: {message}")));
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
            self.render_safe(&IOOutput::error(format!("Failed to send message: {e:?}")));
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
                self.render_safe(&IOOutput::error(format!(
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
                self.render_safe(&IOOutput::error(format!(
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
            // Approval feedback is displayed via ClientRunner → IOOutput::ShowApproved

            // Clear pending approval
            if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(id.as_str()) {
                self.pending_approval = None;
            }
        } else {
            self.render_safe(&IOOutput::warn("No pending approval. Use: y <id>"));
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
            // Rejection feedback is displayed via ClientRunner → IOOutput::ShowRejected

            // Clear pending approval
            if self.pending_approval.as_ref().map(|p| p.id.as_str()) == Some(id.as_str()) {
                self.pending_approval = None;
            }
        } else {
            self.render_safe(&IOOutput::warn("No pending approval. Use: n <id> [reason]"));
        }
    }

    /// Shows help text as a single block (avoids per-line terminal redraws).
    fn show_help(&self) {
        self.render_safe(&IOOutput::info(
            "Commands:\n\
             \x20 y [id]          - Approve pending request\n\
             \x20 n [id] [reason] - Reject pending request\n\
             \x20 p / pause       - Pause execution\n\
             \x20 r / resume      - Resume execution\n\
             \x20 s <message>     - Steer with instruction\n\
             \x20 q / quit        - Quit application\n\
             \x20 veto / stop     - Emergency stop",
        ));
    }

    /// Handles IO output from ClientRunner.
    ///
    /// Updates pending_approval when an approval request is received.
    /// Uses ExternalPrinter during interactive mode.
    fn handle_io_output(&mut self, io_out: IOOutput) {
        // Store as pending approval for "y" / "n" without ID
        if let IOOutput::ShowApprovalRequest { ref id, .. } = io_out {
            self.pending_approval = Some(PendingApproval { id: id.clone() });
        }

        self.render_safe(&io_out);
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

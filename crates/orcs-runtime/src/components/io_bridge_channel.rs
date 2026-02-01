//! IO Bridge Channel - Bridge between View and Model layers.
//!
//! The [`IOBridgeChannel`] is a Component that bridges the View layer (IOPort)
//! with the Model layer (EventBus). It handles:
//!
//! - Input: Parsing user input into Signals
//! - Output: Converting display instructions to IOOutput
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         View Layer                              │
//! │  ┌─────────────────┐              ┌─────────────────┐          │
//! │  │  IOInputHandle  │              │ IOOutputHandle  │          │
//! │  └────────┬────────┘              └────────▲────────┘          │
//! └───────────┼────────────────────────────────┼────────────────────┘
//!             │ IOInput                        │ IOOutput
//!             ▼                                │
//! ┌───────────────────────────────────────────────────────────────┐
//! │                   IOBridgeChannel (Bridge)                     │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │                        IOPort                            │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! │                              │                                  │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │  InputParser (parse line → InputCommand → Signal)        │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! │                              │                                  │
//! │                    Component impl                               │
//! └───────────────────────────────────────────────────────────────┘
//!                                │
//!                     Signal / Request
//!                                ▼
//! ┌───────────────────────────────────────────────────────────────┐
//! │                       Model Layer                              │
//! │                        EventBus                                │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_runtime::components::IOBridgeChannel;
//! use orcs_runtime::io::{IOPort, IOInput, IOOutput};
//! use orcs_types::{ChannelId, Principal, PrincipalId};
//!
//! let channel_id = ChannelId::new();
//! let principal = Principal::User(PrincipalId::new());
//!
//! let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
//! let bridge = IOBridgeChannel::new(port, principal);
//!
//! // View layer sends input via input_handle
//! // IOBridgeChannel converts to Signals
//! // View layer receives output via output_handle
//! ```

use crate::io::{IOOutput, IOPort, InputCommand, InputParser};
use orcs_component::{Component, ComponentError, EventCategory, Status};
use orcs_event::{Request, Signal, SignalKind, SignalResponse};
use orcs_types::{ChannelId, ComponentId, Principal, SignalScope};
use serde_json::Value;

/// IO Bridge Channel - Bridge between View and Model layers.
///
/// Owns an IOPort and converts between IO types and internal events.
/// This is a pure bridge - it does not manage HIL state like approval IDs.
///
/// The parser can be injected for testing or customization.
pub struct IOBridgeChannel {
    /// Component identifier.
    id: ComponentId,
    /// Current status.
    status: Status,
    /// IO port for communication with View layer.
    io_port: IOPort,
    /// Principal representing the Human user.
    principal: Principal,
    /// Channel ID this component belongs to.
    channel_id: ChannelId,
    /// Input parser for converting text to commands.
    parser: InputParser,
}

impl IOBridgeChannel {
    /// Creates a new IOBridgeChannel with default parser.
    ///
    /// # Arguments
    ///
    /// * `io_port` - IO port for View communication
    /// * `principal` - Principal representing the Human user
    #[must_use]
    pub fn new(io_port: IOPort, principal: Principal) -> Self {
        Self::with_parser(io_port, principal, InputParser)
    }

    /// Creates a new IOBridgeChannel with a custom parser.
    ///
    /// This allows injecting a custom parser for testing or customization.
    #[must_use]
    pub fn with_parser(io_port: IOPort, principal: Principal, parser: InputParser) -> Self {
        let channel_id = io_port.channel_id();
        Self {
            id: ComponentId::builtin("io-bridge-channel"),
            status: Status::Idle,
            io_port,
            principal,
            channel_id,
            parser,
        }
    }

    /// Creates a new IOBridgeChannel with a custom component ID.
    #[must_use]
    pub fn with_id(id: ComponentId, io_port: IOPort, principal: Principal) -> Self {
        let channel_id = io_port.channel_id();
        Self {
            id,
            status: Status::Idle,
            io_port,
            principal,
            channel_id,
            parser: InputParser,
        }
    }

    /// Returns the channel ID.
    #[must_use]
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Returns the principal.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Converts an InputCommand to a Signal.
    ///
    /// # Arguments
    ///
    /// * `cmd` - The parsed input command
    /// * `default_approval_id` - Fallback approval ID if command has none
    #[must_use]
    pub fn command_to_signal(
        &self,
        cmd: &InputCommand,
        default_approval_id: Option<&str>,
    ) -> Option<Signal> {
        cmd.to_signal(self.principal.clone(), default_approval_id)
    }

    /// Parses a line of input and converts to Signal.
    ///
    /// # Arguments
    ///
    /// * `line` - Raw input line
    /// * `default_approval_id` - Fallback approval ID for HIL responses
    #[must_use]
    pub fn parse_line_to_signal(
        &self,
        line: &str,
        default_approval_id: Option<&str>,
    ) -> Option<Signal> {
        let cmd = self.parser.parse(line);
        self.command_to_signal(&cmd, default_approval_id)
    }

    /// Drains all available input and converts to Signals.
    ///
    /// Returns a vector of Signals ready to be dispatched to EventBus.
    /// Also returns any InputCommands that couldn't be converted
    /// (e.g., Quit, Unknown) for the caller to handle.
    ///
    /// Approval IDs are extracted from the input context provided by View layer.
    pub fn drain_input_to_signals(&mut self) -> (Vec<Signal>, Vec<InputCommand>) {
        let mut signals = Vec::new();
        let mut other_commands = Vec::new();

        for input in self.io_port.drain_input() {
            match input {
                crate::io::IOInput::Line { text, context } => {
                    let cmd = self.parser.parse(&text);
                    // Use approval_id from context (provided by View layer)
                    let approval_id = context.approval_id.as_deref();
                    if let Some(signal) = self.command_to_signal(&cmd, approval_id) {
                        signals.push(signal);
                    } else {
                        // Commands that don't map to signals (Quit, Unknown, etc.)
                        other_commands.push(cmd);
                    }
                }
                crate::io::IOInput::Signal(kind) => {
                    // Pre-parsed signal from View layer
                    let scope = match &kind {
                        SignalKind::Veto => SignalScope::Global,
                        _ => SignalScope::Channel(self.channel_id),
                    };
                    signals.push(Signal::new(kind, scope, self.principal.clone()));
                }
                crate::io::IOInput::Eof => {
                    // EOF is like Quit
                    other_commands.push(InputCommand::Quit);
                }
            }
        }

        (signals, other_commands)
    }

    /// Receives a single input and processes it.
    ///
    /// Async version - waits for input.
    ///
    /// Approval IDs are extracted from the input context provided by View layer.
    ///
    /// # Returns
    ///
    /// - `Some(Ok(signal))` - Input converted to signal
    /// - `Some(Err(cmd))` - Input is a command that doesn't map to signal
    /// - `None` - IO port closed
    pub async fn recv_input(&mut self) -> Option<Result<Signal, InputCommand>> {
        let input = self.io_port.recv().await?;

        match input {
            crate::io::IOInput::Line { text, context } => {
                let cmd = self.parser.parse(&text);
                // Use approval_id from context (provided by View layer)
                let approval_id = context.approval_id.as_deref();
                if let Some(signal) = self.command_to_signal(&cmd, approval_id) {
                    Some(Ok(signal))
                } else {
                    Some(Err(cmd))
                }
            }
            crate::io::IOInput::Signal(kind) => {
                let scope = match &kind {
                    SignalKind::Veto => SignalScope::Global,
                    _ => SignalScope::Channel(self.channel_id),
                };
                Some(Ok(Signal::new(kind, scope, self.principal.clone())))
            }
            crate::io::IOInput::Eof => Some(Err(InputCommand::Quit)),
        }
    }

    // === Output methods (OutputSink-compatible) ===

    /// Sends an output to the View layer.
    ///
    /// # Errors
    ///
    /// Returns error if the output handle has been dropped.
    pub async fn send_output(
        &self,
        output: IOOutput,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port.send(output).await
    }

    /// Displays an approval request.
    pub async fn show_approval_request(
        &self,
        request: &super::ApprovalRequest,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port.send(IOOutput::approval_request(request)).await
    }

    /// Displays approval confirmation.
    pub async fn show_approved(
        &self,
        approval_id: &str,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port.send(IOOutput::approved(approval_id)).await
    }

    /// Displays rejection confirmation.
    pub async fn show_rejected(
        &self,
        approval_id: &str,
        reason: Option<&str>,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port
            .send(IOOutput::rejected(approval_id, reason.map(String::from)))
            .await
    }

    /// Displays an info message.
    pub async fn info(
        &self,
        message: &str,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port.send(IOOutput::info(message)).await
    }

    /// Displays a warning message.
    pub async fn warn(
        &self,
        message: &str,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port.send(IOOutput::warn(message)).await
    }

    /// Displays an error message.
    pub async fn error(
        &self,
        message: &str,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port.send(IOOutput::error(message)).await
    }

    /// Displays a prompt message.
    pub async fn prompt(
        &self,
        message: &str,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<IOOutput>> {
        self.io_port.send(IOOutput::prompt(message)).await
    }

    /// Returns `true` if the output channel is closed.
    #[must_use]
    pub fn is_output_closed(&self) -> bool {
        self.io_port.is_output_closed()
    }
}

impl Component for IOBridgeChannel {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn subscriptions(&self) -> &[EventCategory] {
        // Subscribe to Hil for approval notifications, Lifecycle for system events
        &[EventCategory::Hil, EventCategory::Lifecycle]
    }

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
        match request.operation.as_str() {
            "display" => {
                // Display a message (from other components)
                let message = request
                    .payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ComponentError::InvalidPayload("Missing message".into()))?;

                let style = request
                    .payload
                    .get("style")
                    .and_then(|v| v.as_str())
                    .unwrap_or("info");

                let output = match style {
                    "error" => IOOutput::error(message),
                    "warn" => IOOutput::warn(message),
                    "success" => IOOutput::success(message),
                    "debug" => IOOutput::debug(message),
                    _ => IOOutput::info(message),
                };

                // Use try_send since we're in a sync context
                self.io_port
                    .try_send(output)
                    .map_err(|e| ComponentError::ExecutionFailed(format!("Send failed: {}", e)))?;

                Ok(Value::Null)
            }
            "prompt" => {
                // Display a prompt
                let message = request
                    .payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ComponentError::InvalidPayload("Missing message".into()))?;

                self.io_port
                    .try_send(IOOutput::prompt(message))
                    .map_err(|e| ComponentError::ExecutionFailed(format!("Send failed: {}", e)))?;

                Ok(Value::Null)
            }
            _ => Err(ComponentError::NotSupported(request.operation.clone())),
        }
    }

    fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
        match &signal.kind {
            SignalKind::Veto => {
                self.abort();
                SignalResponse::Abort
            }
            SignalKind::Cancel => {
                if signal.affects_channel(self.channel_id) {
                    self.abort();
                    SignalResponse::Abort
                } else {
                    SignalResponse::Ignored
                }
            }
            SignalKind::Pause => {
                if signal.affects_channel(self.channel_id) {
                    self.status = Status::Paused;
                    SignalResponse::Handled
                } else {
                    SignalResponse::Ignored
                }
            }
            SignalKind::Resume => {
                if signal.affects_channel(self.channel_id) && matches!(self.status, Status::Paused)
                {
                    self.status = Status::Idle;
                    SignalResponse::Handled
                } else {
                    SignalResponse::Ignored
                }
            }
            _ => SignalResponse::Ignored,
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;
    }

    fn init(&mut self) -> Result<(), ComponentError> {
        self.status = Status::Idle;
        Ok(())
    }

    fn shutdown(&mut self) {
        self.status = Status::Completed;
    }
}

impl std::fmt::Debug for IOBridgeChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IOBridgeChannel")
            .field("id", &self.id)
            .field("status", &self.status)
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::IOInput;
    use orcs_types::PrincipalId;

    fn test_principal() -> Principal {
        Principal::User(PrincipalId::new())
    }

    fn setup() -> (
        IOBridgeChannel,
        crate::io::IOInputHandle,
        crate::io::IOOutputHandle,
    ) {
        let channel_id = ChannelId::new();
        let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
        let channel = IOBridgeChannel::new(port, test_principal());
        (channel, input_handle, output_handle)
    }

    #[test]
    fn io_bridge_channel_creation() {
        let (channel, _, _) = setup();
        assert_eq!(channel.id().name, "io-bridge-channel");
        assert_eq!(channel.status(), Status::Idle);
    }

    #[test]
    fn parse_line_to_signal_approve() {
        let (channel, _, _) = setup();

        // Without default approval ID, approve without ID returns None
        let signal = channel.parse_line_to_signal("y", None);
        assert!(signal.is_none());

        // With explicit ID in input
        let signal = channel.parse_line_to_signal("y req-123", None);
        assert!(signal.is_some());
        assert!(signal.unwrap().is_approve());
    }

    #[test]
    fn parse_line_to_signal_with_default() {
        let (channel, _, _) = setup();

        // With default approval ID passed as argument
        let signal = channel.parse_line_to_signal("y", Some("default-id"));
        assert!(signal.is_some());

        let signal = signal.unwrap();
        assert!(signal.is_approve());
        if let SignalKind::Approve { approval_id } = &signal.kind {
            assert_eq!(approval_id, "default-id");
        }
    }

    #[test]
    fn parse_line_to_signal_veto() {
        let (channel, _, _) = setup();

        let signal = channel.parse_line_to_signal("veto", None);
        assert!(signal.is_some());
        assert!(signal.unwrap().is_veto());
    }

    #[test]
    fn parse_line_to_signal_quit_returns_none() {
        let (channel, _, _) = setup();

        // Quit doesn't map to a signal
        let signal = channel.parse_line_to_signal("q", None);
        assert!(signal.is_none());
    }

    #[tokio::test]
    async fn drain_input_to_signals() {
        use crate::io::InputContext;

        let (mut channel, input_handle, _output_handle) = setup();
        let ctx = InputContext::with_approval_id("pending-1");

        // Send various inputs with context
        input_handle
            .send(IOInput::line_with_context("y", ctx.clone()))
            .await
            .unwrap();
        input_handle
            .send(IOInput::line_with_context("n", ctx.clone()))
            .await
            .unwrap();
        input_handle
            .send(IOInput::line_with_context("veto", ctx.clone()))
            .await
            .unwrap();
        input_handle.send(IOInput::line("q")).await.unwrap(); // Quit (no context needed)
        input_handle.send(IOInput::line("unknown")).await.unwrap(); // Unknown

        // Small delay to ensure messages are received
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let (signals, commands) = channel.drain_input_to_signals();

        // y, n, veto -> 3 signals
        assert_eq!(signals.len(), 3);
        assert!(signals[0].is_approve());
        assert!(signals[1].is_reject());
        assert!(signals[2].is_veto());

        // q, unknown -> 2 commands
        assert_eq!(commands.len(), 2);
        assert!(matches!(commands[0], InputCommand::Quit));
        assert!(matches!(commands[1], InputCommand::Unknown { .. }));
    }

    #[tokio::test]
    async fn recv_input_signal() {
        use crate::io::InputContext;

        let (mut channel, input_handle, _output_handle) = setup();
        let ctx = InputContext::with_approval_id("req-1");

        input_handle
            .send(IOInput::line_with_context("y", ctx))
            .await
            .unwrap();

        let result = channel.recv_input().await;
        assert!(result.is_some());
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn recv_input_non_signal() {
        let (mut channel, input_handle, _output_handle) = setup();

        input_handle.send(IOInput::line("q")).await.unwrap();

        let result = channel.recv_input().await;
        assert!(result.is_some());
        let cmd = result.unwrap().unwrap_err();
        assert!(matches!(cmd, InputCommand::Quit));
    }

    #[tokio::test]
    async fn send_output() {
        let (channel, _input_handle, mut output_handle) = setup();

        channel.info("test message").await.unwrap();

        let output = output_handle.recv().await.unwrap();
        assert!(matches!(output, IOOutput::Print { .. }));
    }

    #[tokio::test]
    async fn show_approval_request() {
        let (channel, _input_handle, mut output_handle) = setup();

        let req = super::super::ApprovalRequest::with_id(
            "req-123",
            "write",
            "Write file",
            serde_json::json!({}),
        );
        channel.show_approval_request(&req).await.unwrap();

        let output = output_handle.recv().await.unwrap();
        if let IOOutput::ShowApprovalRequest { id, operation, .. } = output {
            assert_eq!(id, "req-123");
            assert_eq!(operation, "write");
        } else {
            panic!("Expected ShowApprovalRequest");
        }
    }

    #[test]
    fn on_signal_veto() {
        let (mut channel, _, _) = setup();

        let signal = Signal::veto(test_principal());
        let response = channel.on_signal(&signal);

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(channel.status(), Status::Aborted);
    }

    #[test]
    fn on_signal_cancel_own_channel() {
        let (mut channel, _, _) = setup();
        let channel_id = channel.channel_id();

        let signal = Signal::cancel(channel_id, test_principal());
        let response = channel.on_signal(&signal);

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(channel.status(), Status::Aborted);
    }

    #[test]
    fn on_signal_cancel_other_channel() {
        let (mut channel, _, _) = setup();
        let other_channel = ChannelId::new();

        let signal = Signal::cancel(other_channel, test_principal());
        let response = channel.on_signal(&signal);

        assert_eq!(response, SignalResponse::Ignored);
        assert_eq!(channel.status(), Status::Idle);
    }

    #[test]
    fn on_signal_pause_resume() {
        let (mut channel, _, _) = setup();
        let channel_id = channel.channel_id();

        // Pause
        let signal = Signal::pause(channel_id, test_principal());
        let response = channel.on_signal(&signal);
        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(channel.status(), Status::Paused);

        // Resume
        let signal = Signal::resume(channel_id, test_principal());
        let response = channel.on_signal(&signal);
        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(channel.status(), Status::Idle);
    }

    #[test]
    fn on_request_display() {
        let (mut channel, _, mut output_handle) = setup();

        let request = Request::new(
            EventCategory::Lifecycle,
            "display",
            ComponentId::builtin("test"),
            channel.channel_id(),
            serde_json::json!({ "message": "Hello", "style": "info" }),
        );

        let result = channel.on_request(&request);
        assert!(result.is_ok());

        let output = output_handle.try_recv();
        assert!(output.is_some());
    }

    #[test]
    fn on_request_prompt() {
        let (mut channel, _, mut output_handle) = setup();

        let request = Request::new(
            EventCategory::Lifecycle,
            "prompt",
            ComponentId::builtin("test"),
            channel.channel_id(),
            serde_json::json!({ "message": "Enter value:" }),
        );

        let result = channel.on_request(&request);
        assert!(result.is_ok());

        let output = output_handle.try_recv();
        assert!(output.is_some());
        assert!(matches!(output.unwrap(), IOOutput::Prompt { .. }));
    }

    #[test]
    fn init_shutdown() {
        let (mut channel, _, _) = setup();

        channel.init().unwrap();
        assert_eq!(channel.status(), Status::Idle);

        channel.shutdown();
        assert_eq!(channel.status(), Status::Completed);
    }

    #[test]
    fn debug_impl() {
        let (channel, _, _) = setup();
        let debug_str = format!("{:?}", channel);
        assert!(debug_str.contains("IOBridgeChannel"));
        assert!(debug_str.contains("io-bridge-channel"));
    }
}

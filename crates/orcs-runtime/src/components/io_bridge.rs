//! IO Bridge - Bridge between View and Model layers.
//!
//! The [`IOBridge`] bridges the View layer (IOPort) with the Model layer (EventBus).
//! It handles:
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
//! │                      IOBridge (Bridge)                        │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │                        IOPort                            │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! │                              │                                  │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │  InputParser (parse line → InputCommand → Signal)        │  │
//! │  └─────────────────────────────────────────────────────────┘  │
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
//! # Note
//!
//! IOBridge does not hold a Principal. The Principal is provided by the
//! owning ClientChannel when converting input to Signals.
//!
//! # Example
//!
//! ```
//! use orcs_runtime::components::IOBridge;
//! use orcs_runtime::io::{IOPort, IOInput, IOOutput};
//! use orcs_types::ChannelId;
//!
//! let channel_id = ChannelId::new();
//! let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
//! let bridge = IOBridge::new(port);
//!
//! // View layer sends input via input_handle
//! // IOBridge converts to Signals (with Principal from ClientChannel)
//! // View layer receives output via output_handle
//! ```

use crate::io::{IOOutput, IOPort, InputCommand, InputParser};
use orcs_event::{Signal, SignalKind};
use orcs_types::{ChannelId, Principal, SignalScope};

/// IO Bridge - Pure bridge between View and Model layers.
///
/// Converts between IO types (IOInput/IOOutput) and internal events (Signal).
/// This is a stateless transformation layer.
///
/// **Note**: IOBridge does not hold a Principal. The Principal is provided
/// by the owning ClientChannel when converting input to Signals.
///
/// The parser can be injected for testing or customization.
pub struct IOBridge {
    /// IO port for communication with View layer.
    io_port: IOPort,
    /// Channel ID this belongs to.
    channel_id: ChannelId,
    /// Input parser for converting text to commands.
    parser: InputParser,
}

impl IOBridge {
    /// Creates a new IOBridge with default parser.
    ///
    /// # Arguments
    ///
    /// * `io_port` - IO port for View communication
    #[must_use]
    pub fn new(io_port: IOPort) -> Self {
        Self::with_parser(io_port, InputParser)
    }

    /// Creates a new IOBridge with a custom parser.
    ///
    /// This allows injecting a custom parser for testing or customization.
    #[must_use]
    pub fn with_parser(io_port: IOPort, parser: InputParser) -> Self {
        let channel_id = io_port.channel_id();
        Self {
            io_port,
            channel_id,
            parser,
        }
    }

    /// Returns the channel ID.
    #[must_use]
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Converts an InputCommand to a Signal.
    ///
    /// # Arguments
    ///
    /// * `cmd` - The parsed input command
    /// * `principal` - Principal to use for the signal
    /// * `default_approval_id` - Fallback approval ID if command has none
    #[must_use]
    pub fn command_to_signal(
        &self,
        cmd: &InputCommand,
        principal: &Principal,
        default_approval_id: Option<&str>,
    ) -> Option<Signal> {
        cmd.to_signal(principal.clone(), default_approval_id)
    }

    /// Parses a line of input and converts to Signal.
    ///
    /// # Arguments
    ///
    /// * `line` - Raw input line
    /// * `principal` - Principal to use for the signal
    /// * `default_approval_id` - Fallback approval ID for HIL responses
    #[must_use]
    pub fn parse_line_to_signal(
        &self,
        line: &str,
        principal: &Principal,
        default_approval_id: Option<&str>,
    ) -> Option<Signal> {
        let cmd = self.parser.parse(line);
        self.command_to_signal(&cmd, principal, default_approval_id)
    }

    /// Drains all available input and converts to Signals.
    ///
    /// Returns a vector of Signals ready to be dispatched to EventBus.
    /// Also returns any InputCommands that couldn't be converted
    /// (e.g., Quit, Unknown) for the caller to handle.
    ///
    /// Approval IDs are extracted from the input context provided by View layer.
    ///
    /// # Arguments
    ///
    /// * `principal` - Principal to use for signals
    pub fn drain_input_to_signals(
        &mut self,
        principal: &Principal,
    ) -> (Vec<Signal>, Vec<InputCommand>) {
        let mut signals = Vec::new();
        let mut other_commands = Vec::new();

        for input in self.io_port.drain_input() {
            match input {
                crate::io::IOInput::Line { text, context } => {
                    let cmd = self.parser.parse(&text);
                    let approval_id = context.approval_id.as_deref();
                    if let Some(signal) = self.command_to_signal(&cmd, principal, approval_id) {
                        signals.push(signal);
                    } else {
                        other_commands.push(cmd);
                    }
                }
                crate::io::IOInput::Signal(kind) => {
                    let scope = match &kind {
                        SignalKind::Veto => SignalScope::Global,
                        _ => SignalScope::Channel(self.channel_id),
                    };
                    signals.push(Signal::new(kind, scope, principal.clone()));
                }
                crate::io::IOInput::Eof => {
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
    /// # Arguments
    ///
    /// * `principal` - Principal to use for signals
    ///
    /// # Returns
    ///
    /// - `Some(Ok(signal))` - Input converted to signal
    /// - `Some(Err(cmd))` - Input is a command that doesn't map to signal
    /// - `None` - IO port closed
    pub async fn recv_input(
        &mut self,
        principal: &Principal,
    ) -> Option<Result<Signal, InputCommand>> {
        let input = self.io_port.recv().await?;

        match input {
            crate::io::IOInput::Line { text, context } => {
                let cmd = self.parser.parse(&text);
                let approval_id = context.approval_id.as_deref();
                if let Some(signal) = self.command_to_signal(&cmd, principal, approval_id) {
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
                Some(Ok(Signal::new(kind, scope, principal.clone())))
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

impl std::fmt::Debug for IOBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IOBridge")
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
        IOBridge,
        crate::io::IOInputHandle,
        crate::io::IOOutputHandle,
    ) {
        let channel_id = ChannelId::new();
        let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
        let bridge = IOBridge::new(port);
        (bridge, input_handle, output_handle)
    }

    #[test]
    fn io_bridge_creation() {
        let channel_id = ChannelId::new();
        let (port, _, _) = IOPort::with_defaults(channel_id);
        let bridge = IOBridge::new(port);
        assert_eq!(bridge.channel_id(), channel_id);
    }

    #[test]
    fn parse_line_to_signal_approve() {
        let (bridge, _, _) = setup();
        let principal = test_principal();

        // Without default approval ID, approve without ID returns None
        let signal = bridge.parse_line_to_signal("y", &principal, None);
        assert!(signal.is_none());

        // With explicit ID in input
        let signal = bridge.parse_line_to_signal("y req-123", &principal, None);
        assert!(signal.is_some());
        assert!(signal.unwrap().is_approve());
    }

    #[test]
    fn parse_line_to_signal_with_default() {
        let (bridge, _, _) = setup();
        let principal = test_principal();

        // With default approval ID passed as argument
        let signal = bridge.parse_line_to_signal("y", &principal, Some("default-id"));
        assert!(signal.is_some());

        let signal = signal.unwrap();
        assert!(signal.is_approve());
        if let SignalKind::Approve { approval_id } = &signal.kind {
            assert_eq!(approval_id, "default-id");
        }
    }

    #[test]
    fn parse_line_to_signal_veto() {
        let (bridge, _, _) = setup();
        let principal = test_principal();

        let signal = bridge.parse_line_to_signal("veto", &principal, None);
        assert!(signal.is_some());
        assert!(signal.unwrap().is_veto());
    }

    #[test]
    fn parse_line_to_signal_quit_returns_none() {
        let (bridge, _, _) = setup();
        let principal = test_principal();

        // Quit doesn't map to a signal
        let signal = bridge.parse_line_to_signal("q", &principal, None);
        assert!(signal.is_none());
    }

    #[tokio::test]
    async fn drain_input_to_signals() {
        use crate::io::InputContext;

        let (mut bridge, input_handle, _output_handle) = setup();
        let principal = test_principal();
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
        input_handle.send(IOInput::line("q")).await.unwrap();
        input_handle.send(IOInput::line("unknown")).await.unwrap();

        // Small delay to ensure messages are received
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let (signals, commands) = bridge.drain_input_to_signals(&principal);

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

        let (mut bridge, input_handle, _output_handle) = setup();
        let principal = test_principal();
        let ctx = InputContext::with_approval_id("req-1");

        input_handle
            .send(IOInput::line_with_context("y", ctx))
            .await
            .unwrap();

        let result = bridge.recv_input(&principal).await;
        assert!(result.is_some());
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn recv_input_non_signal() {
        let (mut bridge, input_handle, _output_handle) = setup();
        let principal = test_principal();

        input_handle.send(IOInput::line("q")).await.unwrap();

        let result = bridge.recv_input(&principal).await;
        assert!(result.is_some());
        let cmd = result.unwrap().unwrap_err();
        assert!(matches!(cmd, InputCommand::Quit));
    }

    #[tokio::test]
    async fn send_output() {
        let (bridge, _input_handle, mut output_handle) = setup();

        bridge.info("test message").await.unwrap();

        let output = output_handle.recv().await.unwrap();
        assert!(matches!(output, IOOutput::Print { .. }));
    }

    #[tokio::test]
    async fn show_approval_request() {
        let (bridge, _input_handle, mut output_handle) = setup();

        let req = super::super::ApprovalRequest::with_id(
            "req-123",
            "write",
            "Write file",
            serde_json::json!({}),
        );
        bridge.show_approval_request(&req).await.unwrap();

        let output = output_handle.recv().await.unwrap();
        if let IOOutput::ShowApprovalRequest { id, operation, .. } = output {
            assert_eq!(id, "req-123");
            assert_eq!(operation, "write");
        } else {
            panic!("Expected ShowApprovalRequest");
        }
    }

    #[test]
    fn debug_impl() {
        let (bridge, _, _) = setup();
        let debug_str = format!("{:?}", bridge);
        assert!(debug_str.contains("IOBridge"));
        assert!(debug_str.contains("channel_id"));
    }
}

//! IO Port for bidirectional communication between View and Bridge layers.
//!
//! The [`IOPort`] provides async channels for communication:
//!
//! - Input: View → Bridge (user commands, signals)
//! - Output: Bridge → View (display instructions)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         View Layer                              │
//! │  ┌─────────────────┐              ┌─────────────────┐          │
//! │  │  IOInputHandle  │              │ IOOutputHandle  │          │
//! │  │    (sender)     │              │   (receiver)    │          │
//! │  └────────┬────────┘              └────────▲────────┘          │
//! └───────────┼────────────────────────────────┼────────────────────┘
//!             │ IOInput                        │ IOOutput
//!             ▼                                │
//! ┌───────────────────────────────────────────────────────────────┐
//! │                       Bridge Layer                             │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │                        IOPort                            │  │
//! │  │  input_rx: Receiver<IOInput>                             │  │
//! │  │  output_tx: Sender<IOOutput>                             │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! │                              │                                  │
//! │                              ▼                                  │
//! │                      HumanChannel                               │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_runtime::io::{IOPort, IOInput, IOOutput};
//! use orcs_types::ChannelId;
//!
//! #[tokio::main]
//! async fn main() {
//!     let channel_id = ChannelId::new();
//!     let (port, input_handle, mut output_handle) = IOPort::new(channel_id, 32);
//!
//!     // View layer sends input
//!     input_handle.send(IOInput::line("y")).await.unwrap();
//!
//!     // Bridge layer receives and processes
//!     // (in HumanChannel)
//!
//!     // Bridge layer sends output
//!     port.send(IOOutput::info("Approved")).await.unwrap();
//!
//!     // View layer receives output
//!     let output = output_handle.recv().await;
//! }
//! ```

use super::types::{IOInput, IOOutput};
use orcs_types::ChannelId;
use tokio::sync::mpsc;

/// Default buffer size for IO channels.
pub const DEFAULT_BUFFER_SIZE: usize = 64;

/// IO Port for Bridge layer.
///
/// Holds the receiving end of input channel and sending end of output channel.
/// Owned by HumanChannel.
pub struct IOPort {
    /// Channel identifier this port belongs to.
    channel_id: ChannelId,
    /// Receiver for input from View layer.
    input_rx: mpsc::Receiver<IOInput>,
    /// Sender for output to View layer.
    output_tx: mpsc::Sender<IOOutput>,
}

impl IOPort {
    /// Creates a new IOPort with associated handles.
    ///
    /// Returns a tuple of:
    /// - `IOPort`: For the Bridge layer (HumanChannel)
    /// - `IOInputHandle`: For the View layer to send input
    /// - `IOOutputHandle`: For the View layer to receive output
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel this port belongs to
    /// * `buffer_size` - Buffer size for the channels
    #[must_use]
    pub fn new(channel_id: ChannelId, buffer_size: usize) -> (Self, IOInputHandle, IOOutputHandle) {
        let (input_tx, input_rx) = mpsc::channel(buffer_size);
        let (output_tx, output_rx) = mpsc::channel(buffer_size);

        let port = Self {
            channel_id,
            input_rx,
            output_tx,
        };

        let input_handle = IOInputHandle {
            tx: input_tx,
            channel_id,
        };

        let output_handle = IOOutputHandle {
            rx: output_rx,
            channel_id,
        };

        (port, input_handle, output_handle)
    }

    /// Creates a new IOPort with default buffer size.
    #[must_use]
    pub fn with_defaults(channel_id: ChannelId) -> (Self, IOInputHandle, IOOutputHandle) {
        Self::new(channel_id, DEFAULT_BUFFER_SIZE)
    }

    /// Returns the channel ID.
    #[must_use]
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Receives input from View layer (async, waits for input).
    ///
    /// Returns `None` if all input handles have been dropped.
    pub async fn recv(&mut self) -> Option<IOInput> {
        self.input_rx.recv().await
    }

    /// Tries to receive input without blocking.
    ///
    /// Returns `None` if no input is available.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<IOInput> {
        self.input_rx.try_recv().ok()
    }

    /// Sends output to View layer.
    ///
    /// # Errors
    ///
    /// Returns error if the output handle has been dropped.
    pub async fn send(&self, output: IOOutput) -> Result<(), mpsc::error::SendError<IOOutput>> {
        self.output_tx.send(output).await
    }

    /// Tries to send output without blocking.
    ///
    /// # Errors
    ///
    /// Returns error if the channel is full or closed.
    pub fn try_send(&self, output: IOOutput) -> Result<(), mpsc::error::TrySendError<IOOutput>> {
        self.output_tx.try_send(output)
    }

    /// Drains all available input without blocking.
    ///
    /// Useful for batch processing of queued input.
    pub fn drain_input(&mut self) -> Vec<IOInput> {
        let mut inputs = Vec::new();
        while let Some(input) = self.try_recv() {
            inputs.push(input);
        }
        inputs
    }

    /// Returns `true` if the output channel is closed.
    #[must_use]
    pub fn is_output_closed(&self) -> bool {
        self.output_tx.is_closed()
    }
}

impl std::fmt::Debug for IOPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IOPort")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

/// Handle for View layer to send input to Bridge layer.
///
/// Can be cloned to allow multiple input sources.
#[derive(Clone)]
pub struct IOInputHandle {
    tx: mpsc::Sender<IOInput>,
    channel_id: ChannelId,
}

impl IOInputHandle {
    /// Returns the channel ID.
    #[must_use]
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Sends input to Bridge layer.
    ///
    /// # Errors
    ///
    /// Returns error if the IOPort has been dropped.
    pub async fn send(&self, input: IOInput) -> Result<(), mpsc::error::SendError<IOInput>> {
        self.tx.send(input).await
    }

    /// Tries to send input without blocking.
    ///
    /// # Errors
    ///
    /// Returns error if the channel is full or closed.
    pub fn try_send(&self, input: IOInput) -> Result<(), mpsc::error::TrySendError<IOInput>> {
        self.tx.try_send(input)
    }

    /// Sends a line of text.
    ///
    /// Convenience method for `send(IOInput::line(text))`.
    ///
    /// # Errors
    ///
    /// Returns error if the IOPort has been dropped.
    pub async fn send_line(&self, text: impl Into<String>) -> Result<(), mpsc::error::SendError<IOInput>> {
        self.send(IOInput::line(text)).await
    }

    /// Returns `true` if the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

impl std::fmt::Debug for IOInputHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IOInputHandle")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

/// Handle for View layer to receive output from Bridge layer.
pub struct IOOutputHandle {
    rx: mpsc::Receiver<IOOutput>,
    channel_id: ChannelId,
}

impl IOOutputHandle {
    /// Returns the channel ID.
    #[must_use]
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Receives output from Bridge layer (async, waits for output).
    ///
    /// Returns `None` if the IOPort has been dropped.
    pub async fn recv(&mut self) -> Option<IOOutput> {
        self.rx.recv().await
    }

    /// Tries to receive output without blocking.
    ///
    /// Returns `None` if no output is available.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<IOOutput> {
        self.rx.try_recv().ok()
    }

    /// Drains all available output without blocking.
    pub fn drain(&mut self) -> Vec<IOOutput> {
        let mut outputs = Vec::new();
        while let Some(output) = self.try_recv() {
            outputs.push(output);
        }
        outputs
    }
}

impl std::fmt::Debug for IOOutputHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IOOutputHandle")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn port_basic_io() {
        let channel_id = ChannelId::new();
        let (mut port, input_handle, mut output_handle) = IOPort::new(channel_id, 8);

        // Send input from View
        input_handle.send(IOInput::line("hello")).await.unwrap();

        // Receive in Bridge
        let input = port.recv().await.unwrap();
        assert_eq!(input.as_line(), Some("hello"));

        // Send output from Bridge
        port.send(IOOutput::info("received")).await.unwrap();

        // Receive in View
        let output = output_handle.recv().await.unwrap();
        assert!(matches!(output, IOOutput::Print { .. }));
    }

    #[tokio::test]
    async fn port_try_recv_empty() {
        let channel_id = ChannelId::new();
        let (mut port, _input_handle, _output_handle) = IOPort::new(channel_id, 8);

        // No input available
        assert!(port.try_recv().is_none());
    }

    #[tokio::test]
    async fn port_drain_input() {
        let channel_id = ChannelId::new();
        let (mut port, input_handle, _output_handle) = IOPort::new(channel_id, 8);

        // Send multiple inputs
        input_handle.send(IOInput::line("one")).await.unwrap();
        input_handle.send(IOInput::line("two")).await.unwrap();
        input_handle.send(IOInput::line("three")).await.unwrap();

        // Drain all
        let inputs = port.drain_input();
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].as_line(), Some("one"));
        assert_eq!(inputs[1].as_line(), Some("two"));
        assert_eq!(inputs[2].as_line(), Some("three"));

        // No more input
        assert!(port.try_recv().is_none());
    }

    #[tokio::test]
    async fn input_handle_send_line() {
        let channel_id = ChannelId::new();
        let (mut port, input_handle, _output_handle) = IOPort::new(channel_id, 8);

        input_handle.send_line("test").await.unwrap();

        let input = port.recv().await.unwrap();
        assert_eq!(input.as_line(), Some("test"));
    }

    #[tokio::test]
    async fn output_handle_drain() {
        let channel_id = ChannelId::new();
        let (port, _input_handle, mut output_handle) = IOPort::new(channel_id, 8);

        // Send multiple outputs
        port.send(IOOutput::info("one")).await.unwrap();
        port.send(IOOutput::warn("two")).await.unwrap();

        // Drain all
        let outputs = output_handle.drain();
        assert_eq!(outputs.len(), 2);
    }

    #[tokio::test]
    async fn port_channel_id() {
        let channel_id = ChannelId::new();
        let (port, input_handle, output_handle) = IOPort::new(channel_id, 8);

        assert_eq!(port.channel_id(), channel_id);
        assert_eq!(input_handle.channel_id(), channel_id);
        assert_eq!(output_handle.channel_id(), channel_id);
    }

    #[tokio::test]
    async fn port_closed_detection() {
        let channel_id = ChannelId::new();
        let (port, input_handle, _output_handle) = IOPort::new(channel_id, 8);

        assert!(!input_handle.is_closed());
        assert!(!port.is_output_closed());

        // Drop port
        drop(port);

        // Input handle should detect closure
        assert!(input_handle.is_closed());
    }

    #[tokio::test]
    async fn input_handle_clone() {
        let channel_id = ChannelId::new();
        let (mut port, input_handle, _output_handle) = IOPort::new(channel_id, 8);

        let input_handle2 = input_handle.clone();

        // Both handles can send
        input_handle.send_line("from handle 1").await.unwrap();
        input_handle2.send_line("from handle 2").await.unwrap();

        let inputs = port.drain_input();
        assert_eq!(inputs.len(), 2);
    }

    #[test]
    fn port_debug() {
        let channel_id = ChannelId::new();
        let (port, input_handle, output_handle) = IOPort::new(channel_id, 8);

        // Should not panic
        let _ = format!("{:?}", port);
        let _ = format!("{:?}", input_handle);
        let _ = format!("{:?}", output_handle);
    }

    #[tokio::test]
    async fn port_with_defaults() {
        let channel_id = ChannelId::new();
        let (port, _input_handle, _output_handle) = IOPort::with_defaults(channel_id);

        assert_eq!(port.channel_id(), channel_id);
    }
}

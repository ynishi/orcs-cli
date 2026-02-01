//! Console I/O - Complete terminal interaction implementation.
//!
//! Provides [`Console`] which integrates:
//! - [`ConsoleInputReader`] - stdin reading with signal handling
//! - [`ConsoleRenderer`] - terminal output rendering
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────────┐
//! │                         Console                                │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │               ConsoleInputReader                         │  │
//! │  │  - Reads stdin line by line                              │  │
//! │  │  - Handles Ctrl+C → SignalKind::Veto                     │  │
//! │  │  - Sends IOInput to IOInputHandle                        │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! │                              │                                  │
//! │                       IOInputHandle                             │
//! │                              │                                  │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │               ConsoleRenderer                            │  │
//! │  │  - Receives IOOutput from IOOutputHandle                 │  │
//! │  │  - Renders to terminal with styling                      │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```no_run
//! use orcs_runtime::io::{Console, IOPort};
//! use orcs_runtime::components::HumanChannel;
//! use orcs_types::{ChannelId, Principal, PrincipalId};
//!
//! #[tokio::main]
//! async fn main() {
//!     let channel_id = ChannelId::new();
//!     let principal = Principal::User(PrincipalId::new());
//!
//!     // Create IO port and handles
//!     let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
//!
//!     // Create console with handles
//!     let console = Console::new(input_handle, output_handle);
//!
//!     // Create HumanChannel (Bridge)
//!     let human_channel = HumanChannel::new(port, principal);
//!
//!     // Run console (spawns input reader and renderer tasks)
//!     console.run().await;
//! }
//! ```

use super::renderer::ConsoleRenderer;
use super::types::IOInput;
use super::{IOInputHandle, IOOutputHandle};
use orcs_event::SignalKind;
use std::io::BufRead;
use tokio::sync::mpsc;

/// Console input reader.
///
/// Reads lines from stdin and sends them to the IOInputHandle.
/// Also handles Ctrl+C to send Veto signals.
pub struct ConsoleInputReader {
    input_handle: IOInputHandle,
}

impl ConsoleInputReader {
    /// Creates a new console input reader.
    #[must_use]
    pub fn new(input_handle: IOInputHandle) -> Self {
        Self { input_handle }
    }

    /// Runs the input reader loop.
    ///
    /// This is a blocking operation that reads from stdin.
    /// Should be spawned in a blocking task.
    ///
    /// Returns when stdin is closed or an error occurs.
    pub fn run_blocking(self) {
        let stdin = std::io::stdin();
        let handle = stdin.lock();

        for line in handle.lines() {
            match line {
                Ok(line) => {
                    let input = IOInput::line(line);
                    // Use blocking send since we're in a sync context
                    if self.input_handle.try_send(input).is_err() {
                        // Channel closed, exit
                        break;
                    }
                }
                Err(_) => {
                    // EOF or error, send Eof and exit
                    let _ = self.input_handle.try_send(IOInput::Eof);
                    break;
                }
            }
        }
    }

    /// Runs the input reader as an async task.
    ///
    /// Spawns a blocking task internally to read from stdin.
    pub async fn run(self) {
        // Spawn blocking task for stdin reading
        tokio::task::spawn_blocking(move || {
            self.run_blocking();
        })
        .await
        .ok();
    }

    /// Sends a Veto signal.
    ///
    /// Called when Ctrl+C is detected.
    pub async fn send_veto(&self) -> Result<(), mpsc::error::SendError<IOInput>> {
        self.input_handle
            .send(IOInput::Signal(SignalKind::Veto))
            .await
    }

    /// Returns `true` if the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.input_handle.is_closed()
    }
}

impl std::fmt::Debug for ConsoleInputReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleInputReader")
            .field("channel_id", &self.input_handle.channel_id())
            .finish_non_exhaustive()
    }
}

/// Complete console I/O facade.
///
/// Integrates input reading and output rendering for terminal interaction.
pub struct Console {
    input_handle: IOInputHandle,
    output_handle: IOOutputHandle,
    renderer: ConsoleRenderer,
}

impl Console {
    /// Creates a new console.
    #[must_use]
    pub fn new(input_handle: IOInputHandle, output_handle: IOOutputHandle) -> Self {
        Self {
            input_handle,
            output_handle,
            renderer: ConsoleRenderer::new(),
        }
    }

    /// Creates a console with verbose output.
    #[must_use]
    pub fn verbose(input_handle: IOInputHandle, output_handle: IOOutputHandle) -> Self {
        Self {
            input_handle,
            output_handle,
            renderer: ConsoleRenderer::verbose(),
        }
    }

    /// Sets verbose mode for the renderer.
    pub fn set_verbose(&mut self, verbose: bool) {
        self.renderer.set_verbose(verbose);
    }

    /// Runs the console.
    ///
    /// This starts:
    /// 1. Input reader task (reads stdin, sends to IOInputHandle)
    /// 2. Renderer task (receives from IOOutputHandle, renders to terminal)
    ///
    /// Returns when both tasks complete (typically when the channel is closed).
    pub async fn run(self) {
        let input_reader = ConsoleInputReader::new(self.input_handle);
        let renderer = self.renderer;
        let output_handle = self.output_handle;

        // Run input reader and renderer concurrently
        tokio::join!(input_reader.run(), renderer.run(output_handle),);
    }

    /// Spawns the console as background tasks.
    ///
    /// Returns handles to the spawned tasks.
    ///
    /// # Returns
    ///
    /// A tuple of (input_task, renderer_task) JoinHandles.
    pub fn spawn(self) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
        let input_reader = ConsoleInputReader::new(self.input_handle);
        let renderer = self.renderer;
        let output_handle = self.output_handle;

        let input_task = tokio::spawn(async move {
            input_reader.run().await;
        });

        let renderer_task = tokio::spawn(async move {
            renderer.run(output_handle).await;
        });

        (input_task, renderer_task)
    }

    /// Splits the console into its components.
    ///
    /// Useful when you need separate control over input and output.
    #[must_use]
    pub fn split(self) -> (ConsoleInputReader, ConsoleRenderer, IOOutputHandle) {
        (
            ConsoleInputReader::new(self.input_handle),
            self.renderer,
            self.output_handle,
        )
    }
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console")
            .field("channel_id", &self.input_handle.channel_id())
            .field("verbose", &self.renderer.is_verbose())
            .finish_non_exhaustive()
    }
}

/// Ctrl+C handler that sends Veto signal.
///
/// Sets up a Ctrl+C handler that sends a Veto signal through the input handle.
///
/// # Example
///
/// ```no_run
/// use orcs_runtime::io::{IOPort, setup_ctrlc_handler};
/// use orcs_types::ChannelId;
///
/// #[tokio::main]
/// async fn main() {
///     let channel_id = ChannelId::new();
///     let (_port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);
///
///     setup_ctrlc_handler(input_handle.clone());
///
///     // Ctrl+C will now send Veto signal through the input handle
/// }
/// ```
pub fn setup_ctrlc_handler(input_handle: IOInputHandle) {
    // Note: This requires the ctrlc crate or similar
    // For now, we document the pattern but don't implement the actual handler
    // since it requires additional dependencies

    // Example implementation with ctrlc crate:
    // ctrlc::set_handler(move || {
    //     let _ = input_handle.try_send(IOInput::Signal(SignalKind::Veto));
    // }).expect("Error setting Ctrl-C handler");

    // Using tokio signal handling:
    let handle = input_handle;
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            // Send Veto signal
            if handle.try_send(IOInput::Signal(SignalKind::Veto)).is_err() {
                // Channel closed, exit handler
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::IOPort;
    use orcs_types::ChannelId;

    #[test]
    fn console_creation() {
        let channel_id = ChannelId::new();
        let (_, input_handle, output_handle) = IOPort::with_defaults(channel_id);

        let console = Console::new(input_handle, output_handle);
        assert!(!console.renderer.is_verbose());
    }

    #[test]
    fn console_verbose() {
        let channel_id = ChannelId::new();
        let (_, input_handle, output_handle) = IOPort::with_defaults(channel_id);

        let console = Console::verbose(input_handle, output_handle);
        assert!(console.renderer.is_verbose());
    }

    #[test]
    fn console_set_verbose() {
        let channel_id = ChannelId::new();
        let (_, input_handle, output_handle) = IOPort::with_defaults(channel_id);

        let mut console = Console::new(input_handle, output_handle);
        assert!(!console.renderer.is_verbose());

        console.set_verbose(true);
        assert!(console.renderer.is_verbose());
    }

    #[test]
    fn console_split() {
        let channel_id = ChannelId::new();
        // NOTE: `_port` must be kept alive. Using `_` would drop IOPort immediately,
        // closing the channel and causing `input_reader.is_closed()` to return true.
        let (_port, input_handle, output_handle) = IOPort::with_defaults(channel_id);

        let console = Console::new(input_handle, output_handle);
        let (input_reader, renderer, _) = console.split();

        assert!(!input_reader.is_closed());
        assert!(!renderer.is_verbose());
    }

    #[test]
    fn console_debug() {
        let channel_id = ChannelId::new();
        let (_, input_handle, output_handle) = IOPort::with_defaults(channel_id);

        let console = Console::new(input_handle, output_handle);
        let debug_str = format!("{:?}", console);
        assert!(debug_str.contains("Console"));
    }

    #[test]
    fn input_reader_debug() {
        let channel_id = ChannelId::new();
        let (_port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);

        let reader = ConsoleInputReader::new(input_handle);
        let debug_str = format!("{:?}", reader);
        assert!(debug_str.contains("ConsoleInputReader"));
    }

    #[tokio::test]
    async fn input_reader_closed_detection() {
        let channel_id = ChannelId::new();
        let (port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);

        let reader = ConsoleInputReader::new(input_handle);
        assert!(!reader.is_closed());

        // Drop port to close channel
        drop(port);

        assert!(reader.is_closed());
    }

    #[tokio::test]
    async fn send_veto() {
        let channel_id = ChannelId::new();
        let (mut port, input_handle, _output_handle) = IOPort::with_defaults(channel_id);

        let reader = ConsoleInputReader::new(input_handle);
        reader.send_veto().await.unwrap();

        let input = port.recv().await.unwrap();
        assert!(matches!(input, IOInput::Signal(SignalKind::Veto)));
    }
}

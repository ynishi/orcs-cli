//! Console Renderer - View layer implementation for terminal output.
//!
//! The [`ConsoleRenderer`] receives [`IOOutput`] from the Bridge layer
//! and renders it to the terminal using tracing and eprintln.
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────────┐
//! │                    HumanChannel (Bridge)                       │
//! │                           │                                    │
//! │                   IOPort.output_tx                             │
//! └───────────────────────────┼───────────────────────────────────┘
//!                             │ IOOutput
//!                             ▼
//! ┌───────────────────────────────────────────────────────────────┐
//! │                         View Layer                             │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │                   IOOutputHandle                         │  │
//! │  │                   (receives IOOutput)                    │  │
//! │  └─────────────────────────┬───────────────────────────────┘  │
//! │                             │                                  │
//! │  ┌─────────────────────────▼───────────────────────────────┐  │
//! │  │                   ConsoleRenderer                        │  │
//! │  │  - render_output() → eprintln! / tracing                 │  │
//! │  │  - run() → background task                               │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```no_run
//! use orcs_runtime::io::{IOPort, IOOutputHandle, ConsoleRenderer};
//! use orcs_types::ChannelId;
//!
//! #[tokio::main]
//! async fn main() {
//!     let channel_id = ChannelId::new();
//!     let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
//!
//!     // Spawn renderer task
//!     let renderer = ConsoleRenderer::new();
//!     tokio::spawn(renderer.run(output_handle));
//!
//!     // Bridge layer sends output via port
//!     // Renderer receives and displays
//! }
//! ```

use super::types::{IOOutput, OutputStyle};
use super::IOOutputHandle;

/// Console renderer for terminal output.
///
/// Renders [`IOOutput`] messages to the terminal using appropriate
/// formatting and logging.
pub struct ConsoleRenderer {
    /// Show debug messages.
    verbose: bool,
}

impl ConsoleRenderer {
    /// Creates a new console renderer.
    #[must_use]
    pub fn new() -> Self {
        Self { verbose: false }
    }

    /// Creates a console renderer with verbose mode.
    #[must_use]
    pub fn verbose() -> Self {
        Self { verbose: true }
    }

    /// Sets verbose mode.
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    /// Returns `true` if verbose mode is enabled.
    #[must_use]
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }

    /// Renders a single output to the console.
    ///
    /// This is the core rendering logic that can be called directly
    /// or used within the `run` loop.
    pub fn render_output(&self, output: &IOOutput) {
        match output {
            IOOutput::Print { text, style } => {
                self.render_styled_text(text, *style);
            }
            IOOutput::Prompt { message } => {
                eprint!("{} ", message);
            }
            IOOutput::ShowApprovalRequest {
                id,
                operation,
                description,
            } => {
                tracing::info!(
                    target: "hil",
                    approval_id = %id,
                    operation = %operation,
                    "Awaiting approval: {}",
                    description
                );
                eprintln!();
                eprintln!("  [{}] {} - {}", operation, id, description);
                eprintln!("  Enter 'y' to approve, 'n' to reject:");
            }
            IOOutput::ShowApproved { approval_id } => {
                tracing::info!(target: "hil", approval_id = %approval_id, "Approved");
                eprintln!("  \u{2713} Approved: {}", approval_id);
            }
            IOOutput::ShowRejected {
                approval_id,
                reason,
            } => {
                if let Some(reason) = reason {
                    tracing::info!(target: "hil", approval_id = %approval_id, reason = %reason, "Rejected");
                    eprintln!("  \u{2717} Rejected: {} ({})", approval_id, reason);
                } else {
                    tracing::info!(target: "hil", approval_id = %approval_id, "Rejected");
                    eprintln!("  \u{2717} Rejected: {}", approval_id);
                }
            }
            IOOutput::Clear => {
                // ANSI escape code to clear screen
                eprint!("\x1B[2J\x1B[1;1H");
            }
        }
    }

    /// Renders styled text to the console.
    fn render_styled_text(&self, text: &str, style: OutputStyle) {
        match style {
            OutputStyle::Normal => {
                eprintln!("{}", text);
            }
            OutputStyle::Info => {
                tracing::info!("{}", text);
            }
            OutputStyle::Warn => {
                tracing::warn!("{}", text);
            }
            OutputStyle::Error => {
                tracing::error!("{}", text);
            }
            OutputStyle::Success => {
                // Green text using ANSI escape code
                eprintln!("\x1B[32m{}\x1B[0m", text);
            }
            OutputStyle::Debug => {
                if self.verbose {
                    tracing::debug!("{}", text);
                }
            }
        }
    }

    /// Runs the renderer loop, consuming output from the handle.
    ///
    /// This is typically spawned as a background task.
    /// The loop runs until the handle is closed (IOPort dropped).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use orcs_runtime::io::{IOPort, ConsoleRenderer};
    /// use orcs_types::ChannelId;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let channel_id = ChannelId::new();
    ///     let (port, _input_handle, output_handle) = IOPort::with_defaults(channel_id);
    ///
    ///     let renderer = ConsoleRenderer::new();
    ///     tokio::spawn(renderer.run(output_handle));
    /// }
    /// ```
    pub async fn run(self, mut output_handle: IOOutputHandle) {
        while let Some(output) = output_handle.recv().await {
            self.render_output(&output);
        }
    }

    /// Drains and renders all available output without blocking.
    ///
    /// Returns the number of outputs rendered.
    pub fn drain_and_render(&self, output_handle: &mut IOOutputHandle) -> usize {
        let outputs = output_handle.drain();
        for output in &outputs {
            self.render_output(output);
        }
        outputs.len()
    }
}

impl Default for ConsoleRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ConsoleRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleRenderer")
            .field("verbose", &self.verbose)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_creation() {
        let renderer = ConsoleRenderer::new();
        assert!(!renderer.is_verbose());

        let renderer = ConsoleRenderer::verbose();
        assert!(renderer.is_verbose());
    }

    #[test]
    fn renderer_set_verbose() {
        let mut renderer = ConsoleRenderer::new();
        assert!(!renderer.is_verbose());

        renderer.set_verbose(true);
        assert!(renderer.is_verbose());
    }

    #[test]
    fn renderer_default() {
        let renderer = ConsoleRenderer::default();
        assert!(!renderer.is_verbose());
    }

    #[test]
    fn renderer_debug() {
        let renderer = ConsoleRenderer::new();
        let debug_str = format!("{:?}", renderer);
        assert!(debug_str.contains("ConsoleRenderer"));
        assert!(debug_str.contains("verbose"));
    }

    // Note: Actually testing render_output would require capturing stderr,
    // which is complex. The important thing is that the code compiles and
    // the logic is exercised through integration tests.

    #[tokio::test]
    async fn renderer_drain_and_render() {
        use crate::io::IOPort;
        use orcs_types::ChannelId;

        let channel_id = ChannelId::new();
        let (port, _input_handle, mut output_handle) = IOPort::with_defaults(channel_id);

        // Send some outputs
        port.send(IOOutput::info("test1")).await.unwrap();
        port.send(IOOutput::info("test2")).await.unwrap();

        // Small delay to ensure messages are received
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let renderer = ConsoleRenderer::new();
        let count = renderer.drain_and_render(&mut output_handle);
        assert_eq!(count, 2);
    }
}

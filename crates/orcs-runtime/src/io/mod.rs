//! I/O abstraction for Human interaction.
//!
//! This module provides the View layer abstraction for ORCS,
//! enabling pluggable I/O backends (Console, WebSocket, etc.).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         View Layer                              │
//! │  Console, WebSocket, GUI, etc.                                  │
//! │  ┌─────────────────┐              ┌─────────────────┐          │
//! │  │  IOInputHandle  │              │ IOOutputHandle  │          │
//! │  │   (send input)  │              │ (receive output)│          │
//! │  └────────┬────────┘              └────────▲────────┘          │
//! └───────────┼────────────────────────────────┼────────────────────┘
//!             │ IOInput                        │ IOOutput
//!             ▼                                │
//! ┌───────────────────────────────────────────────────────────────┐
//! │                       Bridge Layer                             │
//! │  ┌─────────────────────────────────────────────────────────┐  │
//! │  │                        IOPort                            │  │
//! │  │  input_rx ◄── IOInput                                    │  │
//! │  │  output_tx ──► IOOutput                                  │  │
//! │  └─────────────────────────────────────────────────────────┘  │
//! │                              │                                  │
//! │                  IOBridgeChannel (Component)                    │
//! │                              │                                  │
//! │                    Signal / Request                             │
//! └───────────────────────────────────────────────────────────────┘
//!                                │
//!                                ▼
//! ┌───────────────────────────────────────────────────────────────┐
//! │                       Model Layer                              │
//! │                        EventBus                                │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Module Structure
//!
//! - [`types`] - IO types ([`IOInput`], [`IOOutput`], [`OutputStyle`])
//! - [`port`] - IO port and handles ([`IOPort`], [`IOInputHandle`], [`IOOutputHandle`])
//! - [`input`] - Input parsing ([`HumanInput`], [`InputCommand`])
//! - [`output`] - Output rendering ([`OutputSink`], [`ConsoleOutput`])
//!
//! # Example
//!
//! ```
//! use orcs_runtime::io::{IOPort, IOInput, IOOutput, OutputStyle};
//! use orcs_types::ChannelId;
//!
//! // Create IO port
//! let channel_id = ChannelId::new();
//! let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);
//!
//! // View layer uses handles
//! // Bridge layer (HumanChannel) uses port
//! ```

mod console;
mod input;
mod output;
mod parser;
mod port;
mod renderer;
mod types;

// Types (View layer contract)
pub use types::{IOInput, IOOutput, OutputStyle};

// Port (Bridge layer)
pub use port::{IOInputHandle, IOOutputHandle, IOPort, DEFAULT_BUFFER_SIZE};

// Console (View layer - complete terminal I/O)
pub use console::{setup_ctrlc_handler, Console, ConsoleInputReader};
pub use renderer::ConsoleRenderer;

// Input parsing (stateless)
pub use input::InputCommand;
pub use parser::InputParser;

// Legacy/compatibility (to be migrated)
pub use input::HumanInput;
pub use output::{ConsoleOutput, OutputSink};

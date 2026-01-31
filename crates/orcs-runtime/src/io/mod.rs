//! I/O utilities for Human interaction.
//!
//! This module provides:
//!
//! - [`HumanInput`] - stdin signal input parsing
//! - [`OutputSink`] - output display trait

mod input;
mod output;

pub use input::{HumanInput, InputCommand};
pub use output::{ConsoleOutput, OutputSink};

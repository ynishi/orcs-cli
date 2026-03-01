//! Tracing writers for independent terminal and file log layers.
//!
//! Two separate [`MakeWriter`](tracing_subscriber::fmt::MakeWriter) implementations
//! allow independent `EnvFilter` per output target:
//!
//! - [`TerminalMakeWriter`]: routes through [`SharedPrinterSlot`] (interactive) or
//!   falls back to stdout. Non-blocking — drops messages when channel is full.
//! - [`FileMakeWriter`]: appends to the persistent log file. File layer should be
//!   configured with `.with_ansi(false)` so no ANSI stripping is needed.

use orcs_app::{PrintResult, SharedPrinterSlot};
use parking_lot::Mutex;
use std::io::{self, Write};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Terminal layer writer
// ---------------------------------------------------------------------------

/// [`MakeWriter`](tracing_subscriber::fmt::MakeWriter) for the terminal layer.
///
/// Routes log output through rustyline's `ExternalPrinter` (interactive mode)
/// or falls back to stdout. When the intermediary channel is full, messages
/// are silently dropped from terminal display (the file layer retains them).
#[derive(Clone)]
pub struct TerminalMakeWriter {
    slot: SharedPrinterSlot,
}

impl TerminalMakeWriter {
    pub fn new(slot: &SharedPrinterSlot) -> Self {
        Self { slot: slot.clone() }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TerminalMakeWriter {
    type Writer = TerminalWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TerminalWriter {
            slot: self.slot.clone(),
            buf: Vec::with_capacity(256),
        }
    }
}

/// Per-event writer for terminal output.
///
/// Buffers bytes from the tracing formatter. On [`Drop`], sends
/// the buffer through the non-blocking printer slot.
pub struct TerminalWriter {
    slot: SharedPrinterSlot,
    buf: Vec<u8>,
}

impl Write for TerminalWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TerminalWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }

        let msg = String::from_utf8_lossy(&self.buf).into_owned();
        match self.slot.try_print(msg) {
            PrintResult::Sent => {}
            PrintResult::NoPrinter => {
                let _ = io::stdout().write_all(&self.buf);
                let _ = io::stdout().flush();
            }
            PrintResult::Dropped => {
                // Channel full — silently dropped from terminal.
                // The file layer retains all messages independently.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// File layer writer
// ---------------------------------------------------------------------------

/// [`MakeWriter`](tracing_subscriber::fmt::MakeWriter) for the file layer.
///
/// Appends every log line to the persistent log file. The file layer
/// should be constructed with `.with_ansi(false)` so the formatter
/// never emits ANSI escape codes.
#[derive(Clone)]
pub struct FileMakeWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl FileMakeWriter {
    pub fn new(file: Arc<Mutex<std::fs::File>>) -> Self {
        Self { file }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileMakeWriter {
    type Writer = FileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FileWriter {
            file: Arc::clone(&self.file),
            buf: Vec::with_capacity(256),
        }
    }
}

/// Per-event writer for file output.
///
/// Buffers bytes from the tracing formatter. On [`Drop`], appends
/// the buffer to the log file under a lock.
pub struct FileWriter {
    file: Arc<Mutex<std::fs::File>>,
    buf: Vec<u8>,
}

impl Write for FileWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }

        let mut file = self.file.lock();
        let _ = file.write_all(&self.buf);
        let _ = file.flush();
    }
}

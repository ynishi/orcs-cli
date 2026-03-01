//! Custom tracing writer that routes log output through rustyline's ExternalPrinter
//! AND simultaneously writes to a persistent log file.
//!
//! **Terminal output**: When the shared printer slot contains a printer
//! (interactive mode), log lines are sent through it via non-blocking
//! `try_print()`. When the slot is empty (startup, shutdown, non-interactive),
//! output falls back to stdout. When the intermediary channel is full,
//! messages are silently dropped from terminal (the log file retains all).
//!
//! **File output**: Every log line is appended to the log file regardless of
//! printer state. This ensures no information is lost.

use orcs_app::{PrintResult, SharedPrinterSlot};
use parking_lot::Mutex;
use std::io::{self, Write};
use std::sync::Arc;

/// A [`MakeWriter`](tracing_subscriber::fmt::MakeWriter) that dispatches
/// each log event to both:
/// 1. Non-blocking ExternalPrinter (or stdout fallback) — terminal display
/// 2. Log file — persistent record
#[derive(Clone)]
pub struct TracingMakeWriter {
    slot: SharedPrinterSlot,
    log_file: Option<Arc<Mutex<std::fs::File>>>,
}

impl TracingMakeWriter {
    /// Creates a new writer from a [`SharedPrinterSlot`] and an optional log file.
    pub fn new(slot: &SharedPrinterSlot, log_file: Option<Arc<Mutex<std::fs::File>>>) -> Self {
        Self {
            slot: slot.clone(),
            log_file,
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TracingMakeWriter {
    type Writer = TracingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TracingWriter {
            slot: self.slot.clone(),
            log_file: self.log_file.as_ref().map(Arc::clone),
            buf: Vec::with_capacity(256),
        }
    }
}

/// Per-event writer returned by [`TracingMakeWriter`].
///
/// Buffers bytes written by the tracing formatter. On [`Drop`], the
/// accumulated buffer is:
/// 1. Sent through non-blocking printer (if available) or written to stdout
/// 2. Appended to the log file (if configured)
pub struct TracingWriter {
    slot: SharedPrinterSlot,
    log_file: Option<Arc<Mutex<std::fs::File>>>,
    buf: Vec<u8>,
}

impl Write for TracingWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TracingWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }

        // 1. Terminal output: non-blocking via SharedPrinterSlot
        {
            let msg = String::from_utf8_lossy(&self.buf).into_owned();
            match self.slot.try_print(msg) {
                PrintResult::Sent => {}
                PrintResult::NoPrinter => {
                    let _ = io::stdout().write_all(&self.buf);
                    let _ = io::stdout().flush();
                }
                PrintResult::Dropped => {
                    // Channel full — silently dropped from terminal.
                    // Log file below still captures it.
                }
            }
        }

        // 2. File output: always append, strip ANSI escape codes for clean log
        if let Some(ref file_mutex) = self.log_file {
            let clean = strip_ansi(&self.buf);
            let mut file = file_mutex.lock();
            let _ = file.write_all(&clean);
            let _ = file.flush();
        }
    }
}

/// Strips ANSI escape sequences (CSI sequences: ESC `[` ... `<final byte>`).
fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'[' {
            // Skip ESC [ ... until final byte (0x40-0x7E)
            i += 2;
            while i < input.len() && !(0x40..=0x7E).contains(&input[i]) {
                i += 1;
            }
            if i < input.len() {
                i += 1; // skip final byte
            }
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

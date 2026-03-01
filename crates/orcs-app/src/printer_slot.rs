//! Shared printer slot for routing output through rustyline's ExternalPrinter.
//!
//! This slot is shared between:
//! - [`OrcsApp`](crate::OrcsApp) — sets/clears the printer during interactive mode
//! - The tracing `MakeWriter` (in `orcs-cli`) — routes log output through the printer
//!
//! When the slot is empty, consumers fall back to stderr.
//!
//! # Non-blocking guarantee
//!
//! [`NonBlockingPrinter`] wraps the raw `ExternalPrinter` with a bounded
//! intermediary channel and a dedicated drain thread. This ensures that
//! callers (tracing subscriber, render_safe) **never block** on terminal
//! output, preventing deadlocks when DEBUG-level logging saturates
//! rustyline's internal bounded channel.

use parking_lot::Mutex;
use rustyline::ExternalPrinter;
use std::sync::Arc;

/// Result of attempting to print through the shared slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintResult {
    /// Message was enqueued for terminal display.
    Sent,
    /// No printer is installed (startup, shutdown, non-interactive).
    /// Caller should fall back to direct output.
    NoPrinter,
    /// Printer is installed but the intermediary channel is full.
    /// Message is dropped from terminal; log file retains it.
    Dropped,
}

/// Non-blocking wrapper around rustyline's [`ExternalPrinter`].
///
/// A bounded intermediary channel (`SyncSender`) sits between the caller
/// and the actual `ExternalPrinter`. A dedicated drain thread forwards
/// messages from the channel to `ExternalPrinter::print()`.
///
/// - `try_print()` uses `try_send()` — returns immediately, never blocks.
/// - The drain thread may block on `ExternalPrinter::print()` (rustyline's
///   internal bounded channel), but this is a dedicated thread and does
///   not affect the async runtime or readline thread.
/// - On [`Drop`], the `SyncSender` is dropped, causing the drain thread's
///   `recv()` to return `Err` and the thread to exit.
struct NonBlockingPrinter {
    tx: std::sync::mpsc::SyncSender<String>,
}

impl NonBlockingPrinter {
    /// Capacity of the intermediary channel.
    ///
    /// 500 balances memory usage (~500 log lines buffered) vs. burst
    /// tolerance during component initialization.
    const CHANNEL_CAPACITY: usize = 500;

    fn new(mut printer: Box<dyn ExternalPrinter + Send>) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(Self::CHANNEL_CAPACITY);

        std::thread::Builder::new()
            .name("orcs-printer-drain".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    // ExternalPrinter::print() may block when rustyline's
                    // internal channel is full — acceptable here because
                    // this is a dedicated thread, not the async runtime.
                    let _ = printer.print(msg);
                }
            })
            .expect("failed to spawn printer drain thread");

        Self { tx }
    }

    /// Attempts to enqueue a message without blocking.
    ///
    /// Returns `true` if sent, `false` if channel is full (message dropped).
    fn try_print(&self, msg: String) -> bool {
        self.tx.try_send(msg).is_ok()
    }
}

/// Shared slot holding an optional [`NonBlockingPrinter`].
///
/// Created before tracing subscriber init, shared with both the
/// tracing `MakeWriter` and [`OrcsApp`](crate::OrcsApp).
///
/// - **Printer set** → tracing and IOOutput go through NonBlockingPrinter
///   (preserves rustyline prompt, never blocks)
/// - **Printer empty** → fallback to stderr (startup, shutdown, non-interactive)
#[derive(Clone)]
pub struct SharedPrinterSlot {
    inner: Arc<Mutex<Option<NonBlockingPrinter>>>,
}

impl SharedPrinterSlot {
    /// Creates an empty slot (no printer attached).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Installs an ExternalPrinter into the slot, wrapped in a
    /// [`NonBlockingPrinter`] for deadlock-free operation.
    ///
    /// All subsequent `try_print()` calls will route through this printer.
    pub fn set(&self, printer: Box<dyn ExternalPrinter + Send>) {
        *self.inner.lock() = Some(NonBlockingPrinter::new(printer));
    }

    /// Removes the printer from the slot.
    ///
    /// The [`NonBlockingPrinter`] is dropped, which drops the `SyncSender`,
    /// causing the drain thread to exit cleanly.
    pub fn clear(&self) {
        *self.inner.lock() = None;
    }

    /// Attempts to print a message through the non-blocking printer.
    ///
    /// Returns [`PrintResult`] indicating the outcome:
    /// - [`Sent`](PrintResult::Sent) — enqueued for terminal display
    /// - [`NoPrinter`](PrintResult::NoPrinter) — no printer installed
    /// - [`Dropped`](PrintResult::Dropped) — channel full, message dropped
    pub fn try_print(&self, msg: String) -> PrintResult {
        let guard = self.inner.lock();
        if let Some(ref printer) = *guard {
            if printer.try_print(msg) {
                PrintResult::Sent
            } else {
                PrintResult::Dropped
            }
        } else {
            PrintResult::NoPrinter
        }
    }
}

impl Default for SharedPrinterSlot {
    fn default() -> Self {
        Self::new()
    }
}

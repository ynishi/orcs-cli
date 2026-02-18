//! Shared printer slot for routing output through rustyline's ExternalPrinter.
//!
//! This slot is shared between:
//! - [`OrcsApp`](crate::OrcsApp) — sets/clears the printer during interactive mode
//! - The tracing `MakeWriter` (in `orcs-cli`) — routes log output through the printer
//!
//! When the slot is empty, consumers fall back to stderr.

use parking_lot::Mutex;
use rustyline::ExternalPrinter;
use std::sync::Arc;

/// Shared slot holding an optional [`ExternalPrinter`].
///
/// Created before tracing subscriber init, shared with both the
/// tracing `MakeWriter` and [`OrcsApp`](crate::OrcsApp).
///
/// - **Printer set** → tracing and IOOutput go through ExternalPrinter
///   (preserves rustyline prompt)
/// - **Printer empty** → fallback to stderr (startup, shutdown, non-interactive)
#[derive(Clone)]
pub struct SharedPrinterSlot {
    inner: Arc<Mutex<Option<Box<dyn ExternalPrinter + Send>>>>,
}

impl SharedPrinterSlot {
    /// Creates an empty slot (no printer attached).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Installs an ExternalPrinter into the slot.
    ///
    /// All subsequent `print()` calls will route through this printer.
    pub fn set(&self, printer: Box<dyn ExternalPrinter + Send>) {
        *self.inner.lock() = Some(printer);
    }

    /// Removes the ExternalPrinter from the slot.
    ///
    /// Subsequent `print()` calls will return `false` (fallback to stderr).
    pub fn clear(&self) {
        *self.inner.lock() = None;
    }

    /// Attempts to print a message through the ExternalPrinter.
    ///
    /// Returns `true` if the message was sent through the printer,
    /// `false` if no printer is installed (caller should fall back to stderr).
    pub fn print(&self, msg: String) -> bool {
        let mut guard = self.inner.lock();
        if let Some(ref mut printer) = *guard {
            printer.print(msg).is_ok()
        } else {
            false
        }
    }

    /// Returns a raw Arc clone for use by external MakeWriter implementations.
    ///
    /// This avoids exposing the inner Mutex type while allowing the
    /// MakeWriter (in `orcs-cli`) to share the same underlying slot.
    #[must_use]
    pub fn clone_arc(&self) -> Arc<Mutex<Option<Box<dyn ExternalPrinter + Send>>>> {
        Arc::clone(&self.inner)
    }
}

impl Default for SharedPrinterSlot {
    fn default() -> Self {
        Self::new()
    }
}

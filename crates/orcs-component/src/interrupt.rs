//! Interrupt signalling for cancelling long-running Component operations.
//!
//! When a ChannelRunner dispatches `on_request`, it may create an interrupt
//! channel and hand the [`InterruptReceiver`] to the Component via
//! [`Component::set_interrupt`].  A background task watches for Veto/Shutdown
//! signals and calls [`InterruptSender::interrupt`] to abort in-flight HTTP
//! requests (or any other cancellable I/O).
//!
//! The channel is **reusable**: after a soft Veto, [`InterruptReceiver::reset`]
//! clears the flag so subsequent `on_request` calls work normally.
//!
//! Built on `AtomicBool` + `tokio::sync::Notify` — no additional crate
//! dependencies.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// Sender half of the interrupt channel.
///
/// Held by the ChannelRunner's veto-watcher task.  Calling [`interrupt`]
/// sets the flag and wakes all receivers.
#[derive(Clone)]
pub struct InterruptSender {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl InterruptSender {
    /// Signal all receivers that an interrupt has been requested.
    pub fn interrupt(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_one();
    }
}

/// Receiver half of the interrupt channel.
///
/// Components store this (e.g. in Lua `app_data`) and pass it down to
/// I/O layers that support cancellation.
#[derive(Clone)]
pub struct InterruptReceiver {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl InterruptReceiver {
    /// Returns `true` if an interrupt has already been requested.
    pub fn is_interrupted(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Wait asynchronously until an interrupt is requested.
    ///
    /// Returns immediately if already interrupted.
    pub async fn interrupted(&mut self) {
        loop {
            if self.flag.load(Ordering::Acquire) {
                return;
            }
            self.notify.notified().await;
        }
    }

    /// Reset the interrupt flag for reuse.
    ///
    /// Called at the start of each `on_request` so a previous Veto
    /// does not immediately kill the next request.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// Create a paired interrupt sender and receiver.
pub fn interrupt_channel() -> (InterruptSender, InterruptReceiver) {
    let flag = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    (
        InterruptSender {
            flag: Arc::clone(&flag),
            notify: Arc::clone(&notify),
        },
        InterruptReceiver { flag, notify },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interrupt_receiver_detects_signal() {
        let (tx, mut rx) = interrupt_channel();
        assert!(!rx.is_interrupted(), "should start as not interrupted");
        tx.interrupt();
        assert!(rx.is_interrupted(), "should be interrupted after signal");
        // interrupted() should return immediately when already set
        rx.interrupted().await;
    }

    #[tokio::test]
    async fn interrupt_receiver_wakes_on_signal() {
        let (tx, mut rx) = interrupt_channel();

        let handle = tokio::spawn(async move {
            rx.interrupted().await;
            true
        });

        // Give the task time to start waiting
        tokio::task::yield_now().await;
        tx.interrupt();

        let result = handle.await.expect("task should complete");
        assert!(result, "task should have been woken by interrupt");
    }

    #[tokio::test]
    async fn reset_clears_interrupt() {
        let (tx, rx) = interrupt_channel();
        tx.interrupt();
        assert!(rx.is_interrupted());
        rx.reset();
        assert!(!rx.is_interrupted(), "should be cleared after reset");
    }

    #[tokio::test]
    async fn reset_allows_reuse() {
        let (tx, mut rx) = interrupt_channel();

        // First interrupt
        tx.interrupt();
        rx.interrupted().await;
        assert!(rx.is_interrupted());

        // Reset and verify fresh state
        rx.reset();
        assert!(!rx.is_interrupted());

        // Second interrupt should work
        let rx_clone = rx.clone();
        let handle = tokio::spawn(async move {
            let mut r = rx_clone;
            r.interrupted().await;
            true
        });

        tokio::task::yield_now().await;
        tx.interrupt();

        let result = handle.await.expect("task should complete");
        assert!(result, "second interrupt should work after reset");
    }

    #[test]
    fn sender_is_clone() {
        let (tx, _rx) = interrupt_channel();
        let _tx2 = tx.clone();
    }

    #[test]
    fn receiver_is_clone() {
        let (_tx, rx) = interrupt_channel();
        let _rx2 = rx.clone();
    }
}

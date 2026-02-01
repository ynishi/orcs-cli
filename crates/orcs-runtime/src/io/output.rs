//! Output display for Human interaction.
//!
//! Provides the [`OutputSink`] trait for displaying information to the user,
//! and a default [`ConsoleOutput`] implementation for terminal output.
//!
//! # Example
//!
//! ```
//! use orcs_runtime::io::{OutputSink, ConsoleOutput};
//! use orcs_runtime::components::ApprovalRequest;
//!
//! let output = ConsoleOutput::new();
//!
//! // Display an approval request
//! let req = ApprovalRequest::new("write", "Write to config.txt", serde_json::json!({}));
//! output.show_approval_request(&req);
//! ```

use crate::components::ApprovalRequest;

/// Output sink for displaying information to the user.
///
/// Implementations handle formatting and display of various message types.
pub trait OutputSink: Send + Sync {
    /// Display an approval request awaiting Human decision.
    fn show_approval_request(&self, request: &ApprovalRequest);

    /// Display that an approval was granted.
    fn show_approved(&self, approval_id: &str);

    /// Display that an approval was rejected.
    fn show_rejected(&self, approval_id: &str, reason: Option<&str>);

    /// Display an informational message.
    fn info(&self, message: &str);

    /// Display a warning message.
    fn warn(&self, message: &str);

    /// Display an error message.
    fn error(&self, message: &str);

    /// Display a debug message (may be hidden based on verbosity).
    fn debug(&self, message: &str);
}

/// Console output implementation using tracing.
///
/// Formats messages for terminal display with appropriate styling.
pub struct ConsoleOutput {
    /// Show debug messages.
    verbose: bool,
}

impl ConsoleOutput {
    /// Creates a new console output.
    #[must_use]
    pub fn new() -> Self {
        Self { verbose: false }
    }

    /// Creates a console output with verbose mode.
    #[must_use]
    pub fn verbose() -> Self {
        Self { verbose: true }
    }

    /// Sets verbose mode.
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }
}

impl Default for ConsoleOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputSink for ConsoleOutput {
    fn show_approval_request(&self, request: &ApprovalRequest) {
        tracing::info!(
            target: "hil",
            approval_id = %request.id,
            operation = %request.operation,
            "Awaiting approval: {}",
            request.description
        );
        eprintln!();
        eprintln!(
            "  [{}] {} - {}",
            request.operation, request.id, request.description
        );
        eprintln!("  Enter 'y' to approve, 'n' to reject:");
    }

    fn show_approved(&self, approval_id: &str) {
        tracing::info!(target: "hil", approval_id = %approval_id, "Approved");
        eprintln!("  ✓ Approved: {}", approval_id);
    }

    fn show_rejected(&self, approval_id: &str, reason: Option<&str>) {
        if let Some(reason) = reason {
            tracing::info!(target: "hil", approval_id = %approval_id, reason = %reason, "Rejected");
            eprintln!("  ✗ Rejected: {} ({})", approval_id, reason);
        } else {
            tracing::info!(target: "hil", approval_id = %approval_id, "Rejected");
            eprintln!("  ✗ Rejected: {}", approval_id);
        }
    }

    fn info(&self, message: &str) {
        tracing::info!("{}", message);
    }

    fn warn(&self, message: &str) {
        tracing::warn!("{}", message);
    }

    fn error(&self, message: &str) {
        tracing::error!("{}", message);
    }

    fn debug(&self, message: &str) {
        if self.verbose {
            tracing::debug!("{}", message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A no-op output sink for testing.
    struct NullOutput;

    impl OutputSink for NullOutput {
        fn show_approval_request(&self, _request: &ApprovalRequest) {}
        fn show_approved(&self, _approval_id: &str) {}
        fn show_rejected(&self, _approval_id: &str, _reason: Option<&str>) {}
        fn info(&self, _message: &str) {}
        fn warn(&self, _message: &str) {}
        fn error(&self, _message: &str) {}
        fn debug(&self, _message: &str) {}
    }

    /// Test output sink that captures messages.
    struct CaptureOutput {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl CaptureOutput {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let messages = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    messages: Arc::clone(&messages),
                },
                messages,
            )
        }
    }

    impl OutputSink for CaptureOutput {
        fn show_approval_request(&self, request: &ApprovalRequest) {
            self.messages
                .lock()
                .expect("mutex poisoned")
                .push(format!("approval_request:{}", request.id));
        }

        fn show_approved(&self, approval_id: &str) {
            self.messages
                .lock()
                .expect("mutex poisoned")
                .push(format!("approved:{}", approval_id));
        }

        fn show_rejected(&self, approval_id: &str, reason: Option<&str>) {
            let msg = match reason {
                Some(r) => format!("rejected:{}:{}", approval_id, r),
                None => format!("rejected:{}", approval_id),
            };
            self.messages.lock().expect("mutex poisoned").push(msg);
        }

        fn info(&self, message: &str) {
            self.messages
                .lock()
                .expect("mutex poisoned")
                .push(format!("info:{}", message));
        }

        fn warn(&self, message: &str) {
            self.messages
                .lock()
                .expect("mutex poisoned")
                .push(format!("warn:{}", message));
        }

        fn error(&self, message: &str) {
            self.messages
                .lock()
                .expect("mutex poisoned")
                .push(format!("error:{}", message));
        }

        fn debug(&self, message: &str) {
            self.messages
                .lock()
                .expect("mutex poisoned")
                .push(format!("debug:{}", message));
        }
    }

    #[test]
    fn capture_approval_request() {
        let (output, messages) = CaptureOutput::new();
        let req = ApprovalRequest::with_id("req-123", "write", "Write file", serde_json::json!({}));

        output.show_approval_request(&req);

        let msgs = messages.lock().expect("mutex poisoned");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "approval_request:req-123");
    }

    #[test]
    fn capture_approved() {
        let (output, messages) = CaptureOutput::new();

        output.show_approved("req-456");

        let msgs = messages.lock().expect("mutex poisoned");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "approved:req-456");
    }

    #[test]
    fn capture_rejected_with_reason() {
        let (output, messages) = CaptureOutput::new();

        output.show_rejected("req-789", Some("not allowed"));

        let msgs = messages.lock().expect("mutex poisoned");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "rejected:req-789:not allowed");
    }

    #[test]
    fn capture_rejected_without_reason() {
        let (output, messages) = CaptureOutput::new();

        output.show_rejected("req-abc", None);

        let msgs = messages.lock().expect("mutex poisoned");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "rejected:req-abc");
    }

    #[test]
    fn null_output_does_nothing() {
        let output = NullOutput;
        let req = ApprovalRequest::with_id("req", "op", "desc", serde_json::json!({}));

        // These should not panic
        output.show_approval_request(&req);
        output.show_approved("id");
        output.show_rejected("id", Some("reason"));
        output.info("info");
        output.warn("warn");
        output.error("error");
        output.debug("debug");
    }

    #[test]
    fn console_output_creation() {
        let output = ConsoleOutput::new();
        assert!(!output.verbose);

        let output = ConsoleOutput::verbose();
        assert!(output.verbose);
    }
}

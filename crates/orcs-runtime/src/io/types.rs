//! IO types for View layer abstraction.
//!
//! These types define the contract between the View layer (Console, WebSocket, etc.)
//! and the Bridge layer (IOBridge).
//!
//! # Architecture
//!
//! ```text
//! View Layer          Bridge Layer            Model Layer
//! ┌─────────┐        ┌─────────────────┐      ┌──────────┐
//! │ Console │◀──────▶│ IOBridge │◀────▶│ EventBus │
//! └─────────┘        └─────────────────┘      └──────────┘
//!     │                       │
//!     │ IOInput               │ Signal/Request
//!     │ IOOutput              │
//! ```

use crate::components::ApprovalRequest;
use orcs_event::SignalKind;
use serde::{Deserialize, Serialize};

/// Context attached to input by the View layer.
///
/// This allows the View layer to pass UI state (like which approval
/// is currently being displayed) along with the user's input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputContext {
    /// The approval ID currently being displayed/awaited.
    ///
    /// When the user types "y" or "n", this provides the target approval ID.
    pub approval_id: Option<String>,
}

impl InputContext {
    /// Creates an empty context.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates a context with an approval ID.
    #[must_use]
    pub fn with_approval_id(approval_id: impl Into<String>) -> Self {
        Self {
            approval_id: Some(approval_id.into()),
        }
    }
}

/// Input from View layer to Bridge layer.
///
/// Represents raw user input before conversion to internal events.
///
/// # Variants
///
/// - `Line`: Raw text input with context from View layer
/// - `Signal`: Pre-parsed control signal (e.g., Ctrl+C → Veto)
/// - `Eof`: End of input stream (disconnect, stdin closed)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IOInput {
    /// Raw text line from user with View context.
    ///
    /// The line is trimmed but not parsed.
    /// The context contains UI state from the View layer.
    Line {
        /// The input text.
        text: String,
        /// Context from View layer (e.g., current approval ID).
        context: InputContext,
    },

    /// Pre-parsed signal from View layer.
    ///
    /// Used when the View layer can directly detect signals
    /// (e.g., terminal Ctrl+C handler).
    Signal(SignalKind),

    /// End of input stream.
    ///
    /// Indicates the input source has closed (EOF, disconnect).
    Eof,
}

impl IOInput {
    /// Creates a Line input with empty context.
    #[must_use]
    pub fn line(text: impl Into<String>) -> Self {
        Self::Line {
            text: text.into(),
            context: InputContext::empty(),
        }
    }

    /// Creates a Line input with context.
    #[must_use]
    pub fn line_with_context(text: impl Into<String>, context: InputContext) -> Self {
        Self::Line {
            text: text.into(),
            context,
        }
    }

    /// Creates a Signal input.
    #[must_use]
    pub fn signal(kind: SignalKind) -> Self {
        Self::Signal(kind)
    }

    /// Returns `true` if this is EOF.
    #[must_use]
    pub fn is_eof(&self) -> bool {
        matches!(self, Self::Eof)
    }

    /// Returns `true` if this is a line input.
    #[must_use]
    pub fn is_line(&self) -> bool {
        matches!(self, Self::Line { .. })
    }

    /// Returns the line content if this is a Line input.
    #[must_use]
    pub fn as_line(&self) -> Option<&str> {
        match self {
            Self::Line { text, .. } => Some(text),
            _ => None,
        }
    }

    /// Returns the context if this is a Line input.
    #[must_use]
    pub fn context(&self) -> Option<&InputContext> {
        match self {
            Self::Line { context, .. } => Some(context),
            _ => None,
        }
    }

    /// Returns the approval ID from context if available.
    #[must_use]
    pub fn approval_id(&self) -> Option<&str> {
        self.context().and_then(|ctx| ctx.approval_id.as_deref())
    }
}

/// Output from Bridge layer to View layer.
///
/// Represents display instructions for the View layer.
/// The View layer is responsible for rendering these appropriately
/// for its output medium (console, WebSocket, GUI, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IOOutput {
    /// Print text with style.
    ///
    /// Generic text output with styling hints.
    Print {
        /// Text content to display.
        text: String,
        /// Display style.
        style: OutputStyle,
    },

    /// Prompt for user input.
    ///
    /// Displays a prompt message and indicates the system is
    /// waiting for user input.
    Prompt {
        /// Prompt message to display.
        message: String,
    },

    /// Display an approval request.
    ///
    /// Shows a HIL approval request awaiting user decision.
    ShowApprovalRequest {
        /// Approval request ID.
        id: String,
        /// Operation type (e.g., "write", "execute").
        operation: String,
        /// Human-readable description.
        description: String,
    },

    /// Confirm approval was granted.
    ShowApproved {
        /// Approved request ID.
        approval_id: String,
    },

    /// Confirm rejection.
    ShowRejected {
        /// Rejected request ID.
        approval_id: String,
        /// Optional rejection reason.
        reason: Option<String>,
    },

    /// Notify that a component has started processing.
    ///
    /// Displayed to the user so they know the system is working
    /// (especially important for LLM calls that take time).
    ShowProcessing {
        /// Component name (e.g. `"claude_cli"`).
        component: String,
        /// Operation being processed (e.g. `"input"`).
        operation: String,
    },

    /// Clear the display.
    Clear,
}

impl IOOutput {
    /// Creates a Print output with Normal style.
    #[must_use]
    pub fn print(text: impl Into<String>) -> Self {
        Self::Print {
            text: text.into(),
            style: OutputStyle::Normal,
        }
    }

    /// Creates a Print output with Info style.
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self::Print {
            text: text.into(),
            style: OutputStyle::Info,
        }
    }

    /// Creates a Print output with Warn style.
    #[must_use]
    pub fn warn(text: impl Into<String>) -> Self {
        Self::Print {
            text: text.into(),
            style: OutputStyle::Warn,
        }
    }

    /// Creates a Print output with Error style.
    #[must_use]
    pub fn error(text: impl Into<String>) -> Self {
        Self::Print {
            text: text.into(),
            style: OutputStyle::Error,
        }
    }

    /// Creates a Print output with Success style.
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        Self::Print {
            text: text.into(),
            style: OutputStyle::Success,
        }
    }

    /// Creates a Print output with Debug style.
    #[must_use]
    pub fn debug(text: impl Into<String>) -> Self {
        Self::Print {
            text: text.into(),
            style: OutputStyle::Debug,
        }
    }

    /// Creates a Prompt output.
    #[must_use]
    pub fn prompt(message: impl Into<String>) -> Self {
        Self::Prompt {
            message: message.into(),
        }
    }

    /// Creates a ShowApprovalRequest output from an ApprovalRequest.
    #[must_use]
    pub fn approval_request(request: &ApprovalRequest) -> Self {
        Self::ShowApprovalRequest {
            id: request.id.clone(),
            operation: request.operation.clone(),
            description: request.description.clone(),
        }
    }

    /// Creates a ShowApproved output.
    #[must_use]
    pub fn approved(approval_id: impl Into<String>) -> Self {
        Self::ShowApproved {
            approval_id: approval_id.into(),
        }
    }

    /// Creates a ShowProcessing output.
    #[must_use]
    pub fn processing(component: impl Into<String>, operation: impl Into<String>) -> Self {
        Self::ShowProcessing {
            component: component.into(),
            operation: operation.into(),
        }
    }

    /// Creates a ShowRejected output.
    #[must_use]
    pub fn rejected(approval_id: impl Into<String>, reason: Option<String>) -> Self {
        Self::ShowRejected {
            approval_id: approval_id.into(),
            reason,
        }
    }
}

/// Output style for text display.
///
/// Provides hints to the View layer about how to render text.
/// The actual rendering (colors, fonts, etc.) is View-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutputStyle {
    /// Normal text (no special styling).
    #[default]
    Normal,

    /// Informational message.
    Info,

    /// Warning message.
    Warn,

    /// Error message.
    Error,

    /// Success message.
    Success,

    /// Debug message (may be hidden based on verbosity).
    Debug,
}

impl OutputStyle {
    /// Returns `true` if this is an error style.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error)
    }

    /// Returns `true` if this is a warning or error style.
    #[must_use]
    pub fn is_warning_or_error(&self) -> bool {
        matches!(self, Self::Warn | Self::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_input_line() {
        let input = IOInput::line("hello");
        assert!(input.is_line());
        assert!(!input.is_eof());
        assert_eq!(input.as_line(), Some("hello"));
        assert!(input.context().is_some());
        assert!(input.approval_id().is_none());
    }

    #[test]
    fn io_input_line_with_context() {
        let ctx = InputContext::with_approval_id("req-123");
        let input = IOInput::line_with_context("y", ctx);
        assert!(input.is_line());
        assert_eq!(input.as_line(), Some("y"));
        assert_eq!(input.approval_id(), Some("req-123"));
    }

    #[test]
    fn io_input_signal() {
        let input = IOInput::signal(SignalKind::Veto);
        assert!(!input.is_line());
        assert!(!input.is_eof());
        assert!(input.as_line().is_none());
        assert!(input.context().is_none());
    }

    #[test]
    fn io_input_eof() {
        let input = IOInput::Eof;
        assert!(input.is_eof());
        assert!(!input.is_line());
    }

    #[test]
    fn input_context_empty() {
        let ctx = InputContext::empty();
        assert!(ctx.approval_id.is_none());
    }

    #[test]
    fn input_context_with_approval() {
        let ctx = InputContext::with_approval_id("test-id");
        assert_eq!(ctx.approval_id, Some("test-id".to_string()));
    }

    #[test]
    fn io_output_constructors() {
        let print = IOOutput::print("text");
        assert!(matches!(
            print,
            IOOutput::Print {
                style: OutputStyle::Normal,
                ..
            }
        ));

        let info = IOOutput::info("info");
        assert!(matches!(
            info,
            IOOutput::Print {
                style: OutputStyle::Info,
                ..
            }
        ));

        let error = IOOutput::error("error");
        assert!(matches!(
            error,
            IOOutput::Print {
                style: OutputStyle::Error,
                ..
            }
        ));
    }

    #[test]
    fn io_output_prompt() {
        let prompt = IOOutput::prompt("Enter value:");
        assert!(matches!(prompt, IOOutput::Prompt { .. }));
    }

    #[test]
    fn io_output_processing() {
        let processing = IOOutput::processing("claude_cli", "input");
        match processing {
            IOOutput::ShowProcessing {
                component,
                operation,
            } => {
                assert_eq!(component, "claude_cli");
                assert_eq!(operation, "input");
            }
            other => panic!("Expected ShowProcessing, got {:?}", other),
        }
    }

    #[test]
    fn io_output_approval() {
        let approved = IOOutput::approved("req-123");
        assert!(matches!(approved, IOOutput::ShowApproved { .. }));

        let rejected = IOOutput::rejected("req-456", Some("too dangerous".to_string()));
        assert!(matches!(
            rejected,
            IOOutput::ShowRejected {
                ref approval_id,
                reason: Some(ref r)
            } if approval_id == "req-456" && r == "too dangerous"
        ));
    }

    #[test]
    fn output_style_checks() {
        assert!(OutputStyle::Error.is_error());
        assert!(!OutputStyle::Info.is_error());

        assert!(OutputStyle::Error.is_warning_or_error());
        assert!(OutputStyle::Warn.is_warning_or_error());
        assert!(!OutputStyle::Info.is_warning_or_error());
    }

    #[test]
    fn output_style_default() {
        let style = OutputStyle::default();
        assert_eq!(style, OutputStyle::Normal);
    }
}

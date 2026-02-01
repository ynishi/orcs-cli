//! Human input handling (legacy compatibility layer).
//!
//! This module provides backward compatibility for `HumanInput`.
//! New code should use [`super::InputParser`] directly.
//!
//! # Migration
//!
//! ```ignore
//! // Old (deprecated)
//! let input = HumanInput::new();
//! let cmd = input.parse_line("y");
//!
//! // New (preferred)
//! let cmd = InputParser::parse("y");
//! ```

use super::parser::InputParser;
use orcs_event::Signal;
use orcs_types::{Principal, PrincipalId};

/// Parsed input command from Human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputCommand {
    /// Approve a pending request.
    Approve {
        /// Optional specific approval ID. If None, approves the first pending.
        approval_id: Option<String>,
    },

    /// Reject a pending request.
    Reject {
        /// Optional specific approval ID. If None, rejects the first pending.
        approval_id: Option<String>,
        /// Optional reason for rejection.
        reason: Option<String>,
    },

    /// Pause execution.
    Pause,

    /// Resume paused execution.
    Resume,

    /// Steer with a message.
    Steer {
        /// Steering instruction.
        message: String,
    },

    /// Graceful quit.
    Quit,

    /// Emergency stop (Veto).
    Veto,

    /// Empty input (blank line).
    ///
    /// Distinct from Unknown - indicates user pressed Enter without input.
    Empty,

    /// Unknown or invalid input.
    Unknown {
        /// The original input line.
        input: String,
    },
}

impl InputCommand {
    /// Converts the command to a Signal if applicable.
    ///
    /// Some commands (like Quit, Unknown) don't map to signals.
    ///
    /// # Arguments
    ///
    /// * `principal` - The Human principal sending the signal
    /// * `default_approval_id` - Default approval ID if none specified
    #[must_use]
    pub fn to_signal(
        &self,
        principal: Principal,
        default_approval_id: Option<&str>,
    ) -> Option<Signal> {
        match self {
            Self::Approve { approval_id } => {
                let id = approval_id.as_deref().or(default_approval_id)?;
                Some(Signal::approve(id, principal))
            }
            Self::Reject {
                approval_id,
                reason,
            } => {
                let id = approval_id.as_deref().or(default_approval_id)?;
                Some(Signal::reject(id, reason.clone(), principal))
            }
            Self::Veto => Some(Signal::veto(principal)),
            Self::Pause | Self::Resume | Self::Steer { .. } => {
                // These require a channel ID which we don't have here
                None
            }
            Self::Quit | Self::Empty | Self::Unknown { .. } => None,
        }
    }

    /// Returns `true` if this is an approval command.
    #[must_use]
    pub fn is_approval(&self) -> bool {
        matches!(self, Self::Approve { .. })
    }

    /// Returns `true` if this is a rejection command.
    #[must_use]
    pub fn is_rejection(&self) -> bool {
        matches!(self, Self::Reject { .. })
    }

    /// Returns `true` if this is a HIL response (approve or reject).
    #[must_use]
    pub fn is_hil_response(&self) -> bool {
        self.is_approval() || self.is_rejection()
    }
}

/// Human input parser and handler.
///
/// Parses text input from stdin into [`InputCommand`]s.
pub struct HumanInput {
    /// Principal representing the Human user.
    principal: Principal,
}

impl HumanInput {
    /// Creates a new HumanInput handler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            principal: Principal::User(PrincipalId::new()),
        }
    }

    /// Creates a HumanInput with a specific principal.
    #[must_use]
    pub fn with_principal(principal: Principal) -> Self {
        Self { principal }
    }

    /// Returns the Human principal.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Parses a line of input into a command.
    ///
    /// # Arguments
    ///
    /// * `line` - The input line (trimmed)
    ///
    /// # Returns
    ///
    /// The parsed [`InputCommand`].
    ///
    /// # Note
    ///
    /// This delegates to [`InputParser::parse`]. The `&self` parameter is
    /// not used; this method exists for backward compatibility.
    #[must_use]
    pub fn parse_line(&self, line: &str) -> InputCommand {
        InputParser.parse(line)
    }

    /// Parses input and converts to a Signal if applicable.
    ///
    /// Convenience method combining `parse_line` and `to_signal`.
    #[must_use]
    pub fn parse_to_signal(&self, line: &str, default_approval_id: Option<&str>) -> Option<Signal> {
        let cmd = self.parse_line(line);
        cmd.to_signal(self.principal.clone(), default_approval_id)
    }
}

impl Default for HumanInput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_event::SignalKind;

    fn input() -> HumanInput {
        HumanInput::new()
    }

    #[test]
    fn parse_approve_simple() {
        let cmd = input().parse_line("y");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));

        let cmd = input().parse_line("yes");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));

        let cmd = input().parse_line("approve");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));
    }

    #[test]
    fn parse_approve_with_id() {
        let cmd = input().parse_line("y req-123");
        assert!(matches!(
            cmd,
            InputCommand::Approve {
                approval_id: Some(ref id)
            } if id == "req-123"
        ));
    }

    #[test]
    fn parse_reject_simple() {
        let cmd = input().parse_line("n");
        assert!(matches!(
            cmd,
            InputCommand::Reject {
                approval_id: None,
                reason: None
            }
        ));
    }

    #[test]
    fn parse_reject_with_id() {
        let cmd = input().parse_line("n req-456");
        assert!(matches!(
            cmd,
            InputCommand::Reject {
                approval_id: Some(ref id),
                reason: None
            } if id == "req-456"
        ));
    }

    #[test]
    fn parse_reject_with_reason() {
        let cmd = input().parse_line("n req-789 too dangerous");
        assert!(matches!(
            cmd,
            InputCommand::Reject {
                approval_id: Some(ref id),
                reason: Some(ref r)
            } if id == "req-789" && r == "too dangerous"
        ));
    }

    #[test]
    fn parse_pause_resume() {
        assert!(matches!(input().parse_line("p"), InputCommand::Pause));
        assert!(matches!(input().parse_line("pause"), InputCommand::Pause));
        assert!(matches!(input().parse_line("r"), InputCommand::Resume));
        assert!(matches!(input().parse_line("resume"), InputCommand::Resume));
    }

    #[test]
    fn parse_steer() {
        let cmd = input().parse_line("s focus on error handling");
        assert!(matches!(
            cmd,
            InputCommand::Steer { ref message } if message == "focus on error handling"
        ));
    }

    #[test]
    fn parse_steer_empty_is_unknown() {
        // Empty steer message should be treated as unknown
        let cmd = input().parse_line("s");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));

        let cmd = input().parse_line("steer");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));
    }

    #[test]
    fn parse_quit() {
        assert!(matches!(input().parse_line("q"), InputCommand::Quit));
        assert!(matches!(input().parse_line("quit"), InputCommand::Quit));
        assert!(matches!(input().parse_line("exit"), InputCommand::Quit));
    }

    #[test]
    fn parse_veto() {
        assert!(matches!(input().parse_line("veto"), InputCommand::Veto));
        assert!(matches!(input().parse_line("stop"), InputCommand::Veto));
        assert!(matches!(input().parse_line("abort"), InputCommand::Veto));
    }

    #[test]
    fn parse_unknown() {
        let cmd = input().parse_line("foobar");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));
    }

    #[test]
    fn parse_empty() {
        let cmd = input().parse_line("");
        assert!(matches!(cmd, InputCommand::Empty));

        let cmd = input().parse_line("   ");
        assert!(matches!(cmd, InputCommand::Empty));
    }

    #[test]
    fn to_signal_approve() {
        let cmd = InputCommand::Approve {
            approval_id: Some("req-123".to_string()),
        };
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, None);

        assert!(signal.is_some());
        let signal = signal.unwrap();
        assert!(signal.is_approve());
    }

    #[test]
    fn to_signal_approve_with_default() {
        let cmd = InputCommand::Approve { approval_id: None };
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, Some("default-id"));

        assert!(signal.is_some());
        let signal = signal.unwrap();
        assert!(signal.is_approve());
        if let SignalKind::Approve { approval_id } = &signal.kind {
            assert_eq!(approval_id, "default-id");
        }
    }

    #[test]
    fn to_signal_approve_no_id() {
        let cmd = InputCommand::Approve { approval_id: None };
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, None);

        // No approval ID available, should return None
        assert!(signal.is_none());
    }

    #[test]
    fn to_signal_reject() {
        let cmd = InputCommand::Reject {
            approval_id: Some("req-456".to_string()),
            reason: Some("not allowed".to_string()),
        };
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, None);

        assert!(signal.is_some());
        let signal = signal.unwrap();
        assert!(signal.is_reject());
    }

    #[test]
    fn to_signal_veto() {
        let cmd = InputCommand::Veto;
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, None);

        assert!(signal.is_some());
        let signal = signal.unwrap();
        assert!(signal.is_veto());
        assert!(signal.is_global());
    }

    #[test]
    fn to_signal_quit_returns_none() {
        let cmd = InputCommand::Quit;
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, None);

        assert!(signal.is_none());
    }

    #[test]
    fn command_helpers() {
        assert!(InputCommand::Approve { approval_id: None }.is_approval());
        assert!(!InputCommand::Approve { approval_id: None }.is_rejection());
        assert!(InputCommand::Approve { approval_id: None }.is_hil_response());

        assert!(InputCommand::Reject {
            approval_id: None,
            reason: None
        }
        .is_rejection());
        assert!(InputCommand::Reject {
            approval_id: None,
            reason: None
        }
        .is_hil_response());
    }

    #[test]
    fn case_insensitive() {
        assert!(matches!(
            input().parse_line("Y"),
            InputCommand::Approve { .. }
        ));
        assert!(matches!(
            input().parse_line("YES"),
            InputCommand::Approve { .. }
        ));
        assert!(matches!(
            input().parse_line("N"),
            InputCommand::Reject { .. }
        ));
        assert!(matches!(input().parse_line("QUIT"), InputCommand::Quit));
    }
}

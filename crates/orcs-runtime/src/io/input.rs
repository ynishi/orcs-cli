//! Input command types.
//!
//! Defines [`InputCommand`] enum representing parsed user input.
//! Use [`super::InputParser`] to parse text into commands.

use orcs_event::Signal;
use orcs_types::Principal;

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

    /// Direct message to a specific component.
    ///
    /// Format: `@component_name message`
    ///
    /// Routes to the named component only, bypassing broadcast.
    ComponentMessage {
        /// Target component name (lowercase).
        target: String,
        /// Message to send to the component.
        message: String,
    },

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
            Self::Quit | Self::Empty | Self::Unknown { .. } | Self::ComponentMessage { .. } => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::InputParser;
    use orcs_event::SignalKind;
    use orcs_types::PrincipalId;

    #[test]
    fn parse_approve_simple() {
        let cmd = InputParser.parse("y");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));

        let cmd = InputParser.parse("yes");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));

        let cmd = InputParser.parse("approve");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));
    }

    #[test]
    fn parse_approve_with_id() {
        let cmd = InputParser.parse("y req-123");
        assert!(matches!(
            cmd,
            InputCommand::Approve {
                approval_id: Some(ref id)
            } if id == "req-123"
        ));
    }

    #[test]
    fn parse_reject_simple() {
        let cmd = InputParser.parse("n");
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
        let cmd = InputParser.parse("n req-456");
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
        let cmd = InputParser.parse("n req-789 too dangerous");
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
        assert!(matches!(InputParser.parse("p"), InputCommand::Pause));
        assert!(matches!(InputParser.parse("pause"), InputCommand::Pause));
        assert!(matches!(InputParser.parse("r"), InputCommand::Resume));
        assert!(matches!(InputParser.parse("resume"), InputCommand::Resume));
    }

    #[test]
    fn parse_steer() {
        let cmd = InputParser.parse("s focus on error handling");
        assert!(matches!(
            cmd,
            InputCommand::Steer { ref message } if message == "focus on error handling"
        ));
    }

    #[test]
    fn parse_steer_empty_is_unknown() {
        let cmd = InputParser.parse("s");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));

        let cmd = InputParser.parse("steer");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));
    }

    #[test]
    fn parse_quit() {
        assert!(matches!(InputParser.parse("q"), InputCommand::Quit));
        assert!(matches!(InputParser.parse("quit"), InputCommand::Quit));
        assert!(matches!(InputParser.parse("exit"), InputCommand::Quit));
    }

    #[test]
    fn parse_veto() {
        assert!(matches!(InputParser.parse("veto"), InputCommand::Veto));
        assert!(matches!(InputParser.parse("stop"), InputCommand::Veto));
        assert!(matches!(InputParser.parse("abort"), InputCommand::Veto));
    }

    #[test]
    fn parse_unknown() {
        let cmd = InputParser.parse("foobar");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));
    }

    #[test]
    fn parse_empty() {
        let cmd = InputParser.parse("");
        assert!(matches!(cmd, InputCommand::Empty));

        let cmd = InputParser.parse("   ");
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
        let signal = signal.expect("approve command with id should produce signal");
        assert!(signal.is_approve());
    }

    #[test]
    fn to_signal_approve_with_default() {
        let cmd = InputCommand::Approve { approval_id: None };
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, Some("default-id"));

        assert!(signal.is_some());
        let signal = signal.expect("approve command with default id should produce signal");
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
        let signal = signal.expect("reject command with id should produce signal");
        assert!(signal.is_reject());
    }

    #[test]
    fn to_signal_veto() {
        let cmd = InputCommand::Veto;
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, None);

        assert!(signal.is_some());
        let signal = signal.expect("veto command should produce signal");
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
    fn to_signal_component_message_returns_none() {
        let cmd = InputCommand::ComponentMessage {
            target: "shell".to_string(),
            message: "ls".to_string(),
        };
        let principal = Principal::User(PrincipalId::new());
        let signal = cmd.to_signal(principal, None);
        assert!(signal.is_none());
    }

    #[test]
    fn case_insensitive() {
        assert!(matches!(
            InputParser.parse("Y"),
            InputCommand::Approve { .. }
        ));
        assert!(matches!(
            InputParser.parse("YES"),
            InputCommand::Approve { .. }
        ));
        assert!(matches!(
            InputParser.parse("N"),
            InputCommand::Reject { .. }
        ));
        assert!(matches!(InputParser.parse("QUIT"), InputCommand::Quit));
    }
}

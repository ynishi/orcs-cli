//! Stateless input parser.
//!
//! Pure function for parsing user input into [`InputCommand`].
//!
//! # Example
//!
//! ```
//! use orcs_runtime::io::{InputParser, InputCommand};
//!
//! let cmd = InputParser::parse("y");
//! assert!(matches!(cmd, InputCommand::Approve { .. }));
//!
//! let cmd = InputParser::parse("n req-123 too dangerous");
//! assert!(matches!(cmd, InputCommand::Reject { .. }));
//! ```

use super::InputCommand;

/// Maximum number of parts when splitting input line.
/// Format: command [id] [reason/message]
const MAX_INPUT_PARTS: usize = 3;

/// Stateless input parser.
///
/// Converts raw text input into [`InputCommand`].
/// This is a pure function with no internal state.
pub struct InputParser;

impl InputParser {
    /// Parses a line of input into a command.
    ///
    /// This is a pure function - the same input always produces the same output.
    ///
    /// # Arguments
    ///
    /// * `line` - The input line (will be trimmed)
    ///
    /// # Returns
    ///
    /// The parsed [`InputCommand`].
    ///
    /// # Input Format
    ///
    /// | Input | Command | Description |
    /// |-------|---------|-------------|
    /// | `y` or `yes` | Approve | Approve pending request |
    /// | `n` or `no` | Reject | Reject pending request |
    /// | `y <id>` | Approve ID | Approve specific request |
    /// | `n <id> [reason]` | Reject ID | Reject with optional reason |
    /// | `p` or `pause` | Pause | Pause current channel |
    /// | `r` or `resume` | Resume | Resume paused channel |
    /// | `s <msg>` or `steer <msg>` | Steer | Steer with message |
    /// | `q` or `quit` | Quit | Graceful shutdown |
    /// | `veto` | Veto | Emergency stop |
    #[must_use]
    pub fn parse(line: &str) -> InputCommand {
        let line = line.trim();

        if line.is_empty() {
            return InputCommand::Empty;
        }

        let parts: Vec<&str> = line.splitn(MAX_INPUT_PARTS, ' ').collect();
        let cmd = parts[0].to_lowercase();

        match cmd.as_str() {
            // Approval
            "y" | "yes" | "approve" => {
                let approval_id = parts.get(1).map(|s| (*s).to_string());
                InputCommand::Approve { approval_id }
            }

            // Rejection
            "n" | "no" | "reject" => {
                let approval_id = parts.get(1).map(|s| (*s).to_string());
                let reason = parts.get(2).map(|s| (*s).to_string());
                InputCommand::Reject {
                    approval_id,
                    reason,
                }
            }

            // Pause
            "p" | "pause" => InputCommand::Pause,

            // Resume
            "r" | "resume" => InputCommand::Resume,

            // Steer (rest of line is the message)
            "s" | "steer" => {
                let message = if parts.len() > 1 {
                    parts[1..].join(" ")
                } else {
                    // Empty steer message is invalid
                    return InputCommand::Unknown {
                        input: line.to_string(),
                    };
                };
                InputCommand::Steer { message }
            }

            // Quit
            "q" | "quit" | "exit" => InputCommand::Quit,

            // Veto
            "veto" | "stop" | "abort" => InputCommand::Veto,

            // Unknown
            _ => InputCommand::Unknown {
                input: line.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_approve_simple() {
        let cmd = InputParser::parse("y");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));

        let cmd = InputParser::parse("yes");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));

        let cmd = InputParser::parse("approve");
        assert!(matches!(cmd, InputCommand::Approve { approval_id: None }));
    }

    #[test]
    fn parse_approve_with_id() {
        let cmd = InputParser::parse("y req-123");
        assert!(matches!(
            cmd,
            InputCommand::Approve {
                approval_id: Some(ref id)
            } if id == "req-123"
        ));
    }

    #[test]
    fn parse_reject_simple() {
        let cmd = InputParser::parse("n");
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
        let cmd = InputParser::parse("n req-456");
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
        let cmd = InputParser::parse("n req-789 too dangerous");
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
        assert!(matches!(InputParser::parse("p"), InputCommand::Pause));
        assert!(matches!(InputParser::parse("pause"), InputCommand::Pause));
        assert!(matches!(InputParser::parse("r"), InputCommand::Resume));
        assert!(matches!(InputParser::parse("resume"), InputCommand::Resume));
    }

    #[test]
    fn parse_steer() {
        let cmd = InputParser::parse("s focus on error handling");
        assert!(matches!(
            cmd,
            InputCommand::Steer { ref message } if message == "focus on error handling"
        ));
    }

    #[test]
    fn parse_steer_empty_is_unknown() {
        let cmd = InputParser::parse("s");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));

        let cmd = InputParser::parse("steer");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));
    }

    #[test]
    fn parse_quit() {
        assert!(matches!(InputParser::parse("q"), InputCommand::Quit));
        assert!(matches!(InputParser::parse("quit"), InputCommand::Quit));
        assert!(matches!(InputParser::parse("exit"), InputCommand::Quit));
    }

    #[test]
    fn parse_veto() {
        assert!(matches!(InputParser::parse("veto"), InputCommand::Veto));
        assert!(matches!(InputParser::parse("stop"), InputCommand::Veto));
        assert!(matches!(InputParser::parse("abort"), InputCommand::Veto));
    }

    #[test]
    fn parse_unknown() {
        let cmd = InputParser::parse("foobar");
        assert!(matches!(cmd, InputCommand::Unknown { .. }));
    }

    #[test]
    fn parse_empty() {
        let cmd = InputParser::parse("");
        assert!(matches!(cmd, InputCommand::Empty));

        let cmd = InputParser::parse("   ");
        assert!(matches!(cmd, InputCommand::Empty));
    }

    #[test]
    fn case_insensitive() {
        assert!(matches!(
            InputParser::parse("Y"),
            InputCommand::Approve { .. }
        ));
        assert!(matches!(
            InputParser::parse("YES"),
            InputCommand::Approve { .. }
        ));
        assert!(matches!(
            InputParser::parse("N"),
            InputCommand::Reject { .. }
        ));
        assert!(matches!(InputParser::parse("QUIT"), InputCommand::Quit));
    }
}

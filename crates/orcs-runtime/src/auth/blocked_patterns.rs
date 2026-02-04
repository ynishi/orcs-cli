//! Command blocking patterns.
//!
//! Defines dangerous command patterns that are blocked or require elevation.
//! These patterns provide defense-in-depth against destructive operations.

/// Dangerous command patterns that are always blocked.
///
/// These patterns are blocked even for elevated sessions because
/// they are either:
/// - Extremely destructive (rm -rf /)
/// - Security risks (eval with untrusted input)
/// - System-level operations that should never be automated
///
/// # Pattern Matching
///
/// Patterns are matched case-insensitively using `contains()`.
/// This means `rm -rf /` will match `RM -RF /` and `sudo rm -rf /`.
pub const BLOCKED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    ":(){ :|:& };:", // Fork bomb
    "> /dev/sda",
    "dd if=/dev/zero of=/dev/sda",
    "mkfs.",
    "chmod -R 777 /",
    "chown -R",
];

/// Dangerous command patterns that require elevated session.
///
/// These patterns are allowed for elevated sessions but blocked
/// for standard sessions. When blocked, they can trigger HIL
/// approval flow via [`CommandCheckResult::RequiresApproval`].
///
/// # Categories
///
/// - **Git destructive**: `reset --hard`, `push --force`, `clean -fd`
/// - **File deletion**: `rm -rf`, `rm -r`
/// - **Shell pipes**: `| sh`, `| bash`
/// - **Privilege escalation**: `sudo`
///
/// [`CommandCheckResult::RequiresApproval`]: crate::auth::CommandCheckResult::RequiresApproval
pub const ELEVATED_REQUIRED_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -r",
    "git reset --hard",
    "git push --force",
    "git push -f",
    "git clean -fd",
    "git checkout .",
    "git restore .",
    "> ", // Redirect (overwrite)
    ">> ",
    "| sh",
    "| bash",
    "curl | sh",
    "wget | sh",
    "sudo ",
];

/// Checks if a command contains any blocked patterns.
///
/// # Arguments
///
/// * `cmd` - The command to check
///
/// # Returns
///
/// `true` if the command matches any blocked pattern.
#[must_use]
pub fn is_blocked_command(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase();
    BLOCKED_PATTERNS
        .iter()
        .any(|pattern| cmd_lower.contains(&pattern.to_lowercase()))
}

/// Checks if a command requires elevated session.
///
/// # Arguments
///
/// * `cmd` - The command to check
///
/// # Returns
///
/// `true` if the command requires elevation.
#[must_use]
pub fn requires_elevation(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase();
    ELEVATED_REQUIRED_PATTERNS
        .iter()
        .any(|pattern| cmd_lower.contains(&pattern.to_lowercase()))
}

/// Returns the matching elevation pattern if the command requires elevation.
///
/// This is used for HIL approval to grant future commands matching the same pattern.
///
/// # Arguments
///
/// * `cmd` - The command to check
///
/// # Returns
///
/// The matching pattern, or `None` if no pattern matches.
#[must_use]
pub fn matching_elevation_pattern(cmd: &str) -> Option<&'static str> {
    let cmd_lower = cmd.to_lowercase();
    ELEVATED_REQUIRED_PATTERNS
        .iter()
        .find(|pattern| cmd_lower.contains(&pattern.to_lowercase()))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_root_deletion() {
        assert!(is_blocked_command("rm -rf /"));
        assert!(is_blocked_command("rm -rf /*"));
        assert!(is_blocked_command("sudo rm -rf /"));
    }

    #[test]
    fn blocked_case_insensitive() {
        assert!(is_blocked_command("RM -RF /"));
        assert!(is_blocked_command("Rm -Rf /"));
    }

    #[test]
    fn blocked_fork_bomb() {
        assert!(is_blocked_command(":(){ :|:& };:"));
    }

    #[test]
    fn blocked_device_write() {
        assert!(is_blocked_command("> /dev/sda"));
        assert!(is_blocked_command("dd if=/dev/zero of=/dev/sda"));
    }

    #[test]
    fn safe_command_not_blocked() {
        assert!(!is_blocked_command("ls -la"));
        assert!(!is_blocked_command("cat file.txt"));
        assert!(!is_blocked_command("rm -rf ./temp")); // Not root
    }

    #[test]
    fn elevation_required_rm() {
        assert!(requires_elevation("rm -rf ./temp"));
        assert!(requires_elevation("rm -r directory"));
    }

    #[test]
    fn elevation_required_git() {
        assert!(requires_elevation("git reset --hard HEAD~1"));
        assert!(requires_elevation("git push --force origin main"));
        assert!(requires_elevation("git push -f origin main"));
    }

    #[test]
    fn elevation_required_sudo() {
        assert!(requires_elevation("sudo apt update"));
    }

    #[test]
    fn safe_command_no_elevation() {
        assert!(!requires_elevation("ls -la"));
        assert!(!requires_elevation("git status"));
        assert!(!requires_elevation("git push origin main"));
    }

    #[test]
    fn matching_pattern_found() {
        assert_eq!(matching_elevation_pattern("rm -rf ./temp"), Some("rm -rf"));
        assert_eq!(
            matching_elevation_pattern("git reset --hard HEAD"),
            Some("git reset --hard")
        );
    }

    #[test]
    fn matching_pattern_not_found() {
        assert_eq!(matching_elevation_pattern("ls -la"), None);
    }
}

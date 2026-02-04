//! Authentication and authorization for ORCS CLI.
//!
//! This crate provides identity and permission management, separating
//! "who is acting" from "what they are allowed to do".
//!
//! # Core Concepts
//!
//! ## Principal (Identity)
//!
//! A [`Principal`] represents the actor performing an action:
//!
//! - [`Principal::User`]: Human user with a [`PrincipalId`](orcs_types::PrincipalId)
//! - [`Principal::Component`]: Component acting autonomously
//! - [`Principal::System`]: Internal system operations
//!
//! ## Privilege Level (Dynamic Permission)
//!
//! [`PrivilegeLevel`] represents the current permission state:
//!
//! - [`PrivilegeLevel::Standard`]: Normal operations only
//! - [`PrivilegeLevel::Elevated`]: Full access (time-limited)
//!
//! This is similar to `sudo` on Unix systems. Even a human user
//! operates in Standard mode by default, preventing accidental
//! destructive operations like `git reset --hard`.
//!
//! ## Session (Principal + Privilege)
//!
//! A [`Session`] combines identity with current privilege level,
//! representing the active security context for operations.
//!
//! # Example
//!
//! ```ignore
//! use orcs_runtime::auth::{Principal, PrivilegeLevel, Session};
//! use orcs_types::PrincipalId;
//! use std::time::Duration;
//!
//! // User starts in Standard mode
//! let user_id = PrincipalId::new();
//! let session = Session::new(Principal::User(user_id));
//!
//! assert!(!session.is_elevated());
//!
//! // Elevate for destructive operations
//! let elevated = session.elevate(Duration::from_secs(300));
//! assert!(elevated.is_elevated());
//! ```
//!
//! # Design Principles
//!
//! ## Separation of Concerns
//!
//! - **Identity** (Principal): Who is acting
//! - **Permission** (PrivilegeLevel): What they can do
//! - **Policy** ([`PermissionChecker`]): Rules for what's allowed
//!
//! ## Permission Checking
//!
//! Use [`PermissionChecker`] trait to check if a session can perform actions:
//!
//! ```ignore
//! use orcs_runtime::auth::{Principal, Session, PermissionChecker, DefaultPolicy};
//! use orcs_types::{PrincipalId, SignalScope};
//!
//! let policy = DefaultPolicy;
//! let session = Session::new(Principal::User(PrincipalId::new()));
//!
//! // Standard session cannot signal Global scope
//! assert!(!policy.can_signal(&session, &SignalScope::Global));
//! ```
//!
//! ## Principle of Least Privilege
//!
//! All actors start in Standard mode. Elevation is:
//!
//! - **Explicit**: Must call [`Session::elevate`]
//! - **Time-limited**: Automatically expires
//! - **Auditable**: Elevation events can be logged
//!
//! ## No Hardcoded Superusers
//!
//! Unlike earlier designs, there is no `SignalSource::Human` with
//! implicit full access. Human users must elevate like any other
//! principal to perform privileged operations.

mod blocked_patterns;
mod checker;
mod command_check;
mod privilege;
mod session;

pub use checker::{DefaultPolicy, PermissionChecker};
pub use command_check::CommandCheckResult;
pub use privilege::PrivilegeLevel;
pub use session::Session;

// Re-export Principal from orcs_types for convenience
pub use orcs_types::Principal;

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::PrincipalId;
    use std::time::Duration;

    #[test]
    fn session_starts_standard() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        assert!(!session.is_elevated());
    }

    #[test]
    fn session_can_elevate() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        let elevated = session.elevate(Duration::from_secs(60));
        assert!(elevated.is_elevated());
    }

    #[test]
    fn session_can_drop_privilege() {
        let session = Session::new(Principal::User(PrincipalId::new()));
        let elevated = session.elevate(Duration::from_secs(60));
        let dropped = elevated.drop_privilege();
        assert!(!dropped.is_elevated());
    }

    #[test]
    fn system_principal() {
        let session = Session::new(Principal::System);
        assert!(!session.is_elevated());
    }
}

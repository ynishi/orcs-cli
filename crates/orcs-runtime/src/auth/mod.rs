//! Authentication and authorization for ORCS CLI.
//!
//! Core types ([`Session`], [`PrivilegeLevel`]) are defined in `orcs-auth`.
//! This module provides runtime-specific implementations:
//!
//! - [`DefaultPolicy`]: Concrete [`PermissionChecker`] with blocked/elevated patterns
//! - [`DefaultGrantStore`]: Concrete [`GrantPolicy`](orcs_auth::GrantPolicy) (in-memory grant store)
//! - [`CommandCheckResult`]: HIL-aware command check result (with `ApprovalRequest`)
//! - Blocked command patterns (always denied, even for elevated)
//! - Elevated command patterns (require HIL approval or elevation)
//!
//! # Architecture
//!
//! ```text
//! orcs-auth (traits + data types)
//!     Session, PrivilegeLevel, PermissionPolicy, GrantPolicy, CommandPermission
//!         ↓
//! orcs-runtime/auth (implementations)
//!     PermissionChecker, DefaultPolicy, DefaultGrantStore, CommandCheckResult
//! ```

pub(crate) mod blocked_patterns;
mod checker;
mod command_check;
mod grant_store;
// Re-export from orcs-auth (thin wrappers for backward compatibility)
mod privilege;
mod session;

pub use checker::{DefaultPolicy, PermissionChecker};
pub use command_check::CommandCheckResult;
pub use grant_store::DefaultGrantStore;
pub use privilege::PrivilegeLevel;
pub use session::Session;

// Re-export from orcs-auth for convenience
pub use orcs_auth::{AccessDenied, CommandPermission, GrantPolicy, PermissionPolicy};

// Re-export Principal from orcs_types for convenience
pub use orcs_types::Principal;

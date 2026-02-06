//! Permission primitives for ORCS.
//!
//! This crate provides the unified permission model for ORCS,
//! sitting at the same level as `orcs-event` in the dependency graph.
//!
//! # Three-Layer Permission Model
//!
//! ```text
//! Effective Permission = Capability(WHAT) ∩ ResourcePolicy(WHERE) ∩ Session(WHO+WHEN)
//! ```
//!
//! | Layer | Type | Controls |
//! |-------|------|----------|
//! | [`Capability`] | Bitflags | What operations are allowed (READ, WRITE, EXECUTE, ...) |
//! | [`SandboxPolicy`] | Trait | Where operations can target (filesystem boundary, etc.) |
//! | [`Session`] + [`PermissionPolicy`] | Struct + Trait | Who is acting, with what privilege level |
//!
//! # Crate Architecture
//!
//! ```text
//! orcs-types  (IDs, Principal)
//!     ↑            ↑
//! orcs-event   orcs-auth  ◄── THIS CRATE
//! (Signal)     (Capability, SandboxPolicy, Session, PermissionPolicy)
//!     ↑            ↑
//!     orcs-component (Component, ChildContext — uses orcs-auth)
//!          ↑
//!     orcs-runtime (ProjectSandbox impl, DefaultPolicy impl)
//! ```
//!
//! # Design Principles
//!
//! - **Trait definitions here, implementations in consumers** — orcs-runtime provides
//!   concrete implementations like `ProjectSandbox` and `DefaultPolicy`
//! - **Resource-general** — `SandboxPolicy` abstracts filesystem today, but the model
//!   extends to Docker volumes, network scopes, etc.
//! - **Deny wins** — A child can never exceed its parent's capabilities

pub mod capability;
pub mod error;
pub mod permission;
pub mod policy;
pub mod privilege;
pub mod resource;
pub mod session;

// Re-export core types
pub use capability::Capability;
pub use error::AccessDenied;
pub use permission::CommandPermission;
pub use policy::PermissionPolicy;
pub use privilege::PrivilegeLevel;
pub use resource::{SandboxError, SandboxPolicy};
pub use session::Session;

// Re-export Principal from orcs_types for convenience
pub use orcs_types::Principal;

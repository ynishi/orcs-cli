//! Principal (actor identity) types.
//!
//! A [`Principal`] represents the actor performing an action,
//! separating "who is acting" from "what they are allowed to do".
//!
//! # Design Rationale
//!
//! Principal is placed in `orcs-types` (not `orcs-auth`) because:
//!
//! 1. **Plugin SDK boundary**: Plugins receive Signals containing Principal
//! 2. **No auth logic dependency**: Principal is pure identity, no permission logic
//! 3. **Avoid circular dependency**: Event -> Auth -> Types would create issues
//!
//! Permission checking (Session, PrivilegeLevel) remains in the runtime layer.

use crate::{ComponentId, PrincipalId};
use serde::{Deserialize, Serialize};

/// The actor performing an action.
///
/// A Principal represents identity only, not permission level.
/// Permission is determined by the runtime layer's Session.
///
/// # Variants
///
/// | Variant | Description | Typical Use |
/// |---------|-------------|-------------|
/// | `User` | Human user | Interactive CLI usage |
/// | `Component` | Autonomous component | Background processing |
/// | `System` | Internal operations | Lifecycle, cleanup |
///
/// # Why Not Just Use ComponentId?
///
/// [`ComponentId`] identifies *what* is executing (LLM, Tool, etc.).
/// [`Principal`] identifies *who* initiated the action.
///
/// A Tool component may execute a file write, but the Principal
/// is the human user who requested it. This distinction enables:
///
/// - Audit trails attributing actions to users
/// - Permission checks based on who initiated
/// - Rate limiting per user, not per component
///
/// # Example
///
/// ```
/// use orcs_types::{Principal, ComponentId, PrincipalId};
///
/// // Human user
/// let user = Principal::User(PrincipalId::new());
///
/// // Component acting autonomously (e.g., scheduled task)
/// let component = Principal::Component(ComponentId::builtin("scheduler"));
///
/// // System internal operation
/// let system = Principal::System;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Principal {
    /// Human user identified by [`PrincipalId`].
    ///
    /// Represents interactive CLI usage or API calls from
    /// authenticated users.
    User(PrincipalId),

    /// Component acting autonomously.
    ///
    /// Used when a component initiates actions without direct
    /// human involvement, such as:
    ///
    /// - Scheduled background tasks
    /// - Event-driven reactions
    /// - Plugin self-maintenance
    Component(ComponentId),

    /// System internal operations.
    ///
    /// Used for operations that are not attributable to any
    /// specific user or component:
    ///
    /// - Lifecycle management (startup, shutdown)
    /// - Garbage collection
    /// - Internal housekeeping
    System,
}

impl Principal {
    /// Returns `true` if this is a [`Principal::User`].
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User(_))
    }

    /// Returns `true` if this is a [`Principal::Component`].
    #[must_use]
    pub fn is_component(&self) -> bool {
        matches!(self, Self::Component(_))
    }

    /// Returns `true` if this is [`Principal::System`].
    #[must_use]
    pub fn is_system(&self) -> bool {
        matches!(self, Self::System)
    }

    /// Returns the [`PrincipalId`] if this is a User, otherwise `None`.
    #[must_use]
    pub fn user_id(&self) -> Option<&PrincipalId> {
        match self {
            Self::User(id) => Some(id),
            _ => None,
        }
    }

    /// Returns the [`ComponentId`] if this is a Component, otherwise `None`.
    #[must_use]
    pub fn component_id(&self) -> Option<&ComponentId> {
        match self {
            Self::Component(id) => Some(id),
            _ => None,
        }
    }
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User(id) => write!(f, "user:{}", id.uuid()),
            Self::Component(id) => write!(f, "component:{}", id.fqn()),
            Self::System => write!(f, "system"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_user() {
        let id = PrincipalId::new();
        let principal = Principal::User(id);

        assert!(principal.is_user());
        assert!(!principal.is_component());
        assert!(!principal.is_system());
        assert!(principal.user_id().is_some());
        assert!(principal.component_id().is_none());
    }

    #[test]
    fn principal_component() {
        let id = ComponentId::builtin("llm");
        let principal = Principal::Component(id);

        assert!(!principal.is_user());
        assert!(principal.is_component());
        assert!(!principal.is_system());
        assert!(principal.user_id().is_none());
        assert!(principal.component_id().is_some());
    }

    #[test]
    fn principal_system() {
        let principal = Principal::System;

        assert!(!principal.is_user());
        assert!(!principal.is_component());
        assert!(principal.is_system());
        assert!(principal.user_id().is_none());
        assert!(principal.component_id().is_none());
    }

    #[test]
    fn principal_display() {
        let user = Principal::User(PrincipalId::new());
        assert!(format!("{user}").starts_with("user:"));

        let component = Principal::Component(ComponentId::builtin("llm"));
        assert_eq!(format!("{component}"), "component:builtin::llm");

        let system = Principal::System;
        assert_eq!(format!("{system}"), "system");
    }

    #[test]
    fn principal_equality() {
        let id1 = PrincipalId::new();
        let id2 = PrincipalId::new();

        let p1 = Principal::User(id1);
        let p2 = Principal::User(id1);
        let p3 = Principal::User(id2);

        assert_eq!(p1, p2);
        assert_ne!(p1, p3);
        assert_ne!(Principal::System, p1);
    }
}

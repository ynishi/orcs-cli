//! Scope types for signal and permission boundaries.

use crate::ChannelId;
use serde::{Deserialize, Serialize};

/// The scope affected by a signal or permission check.
///
/// Defines the boundary of effect for control operations.
///
/// # Scope Hierarchy
///
/// ```text
/// Global
///   └── Channel (specific)
///         └── Children (future)
/// ```
///
/// # Permission Requirements
///
/// | Scope | Standard Privilege | Elevated Privilege |
/// |-------|-------------------|-------------------|
/// | `Global` | Denied | Allowed |
/// | `Channel` (own) | Allowed | Allowed |
/// | `Channel` (other) | Denied | Allowed |
///
/// # Example
///
/// ```
/// use orcs_types::{SignalScope, ChannelId};
///
/// let global = SignalScope::Global;
/// let channel = SignalScope::Channel(ChannelId::new());
///
/// assert!(global.is_global());
/// assert!(!channel.is_global());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalScope {
    /// Affects all channels and components.
    ///
    /// **Requires elevated privilege.**
    Global,

    /// Affects a specific channel and its descendants.
    Channel(ChannelId),
}

impl SignalScope {
    /// Returns `true` if this is a Global scope.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::SignalScope;
    ///
    /// assert!(SignalScope::Global.is_global());
    /// ```
    #[must_use]
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }

    /// Returns `true` if this scope affects the given channel.
    ///
    /// Global scope affects all channels.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::{SignalScope, ChannelId};
    ///
    /// let ch1 = ChannelId::new();
    /// let ch2 = ChannelId::new();
    ///
    /// assert!(SignalScope::Global.affects(ch1));
    /// assert!(SignalScope::Channel(ch1).affects(ch1));
    /// assert!(!SignalScope::Channel(ch1).affects(ch2));
    /// ```
    #[must_use]
    pub fn affects(&self, channel: ChannelId) -> bool {
        match self {
            Self::Global => true,
            Self::Channel(id) => *id == channel,
        }
    }

    /// Returns the target channel if this is a Channel scope.
    #[must_use]
    pub fn channel(&self) -> Option<ChannelId> {
        match self {
            Self::Global => None,
            Self::Channel(id) => Some(*id),
        }
    }
}

impl std::fmt::Display for SignalScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "scope:global"),
            Self::Channel(id) => write!(f, "scope:ch:{}", id.uuid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_scope() {
        let scope = SignalScope::Global;
        assert!(scope.is_global());
        assert!(scope.channel().is_none());
    }

    #[test]
    fn channel_scope() {
        let ch = ChannelId::new();
        let scope = SignalScope::Channel(ch);
        assert!(!scope.is_global());
        assert_eq!(scope.channel(), Some(ch));
    }

    #[test]
    fn affects_global() {
        let ch1 = ChannelId::new();
        let ch2 = ChannelId::new();
        let global = SignalScope::Global;

        assert!(global.affects(ch1));
        assert!(global.affects(ch2));
    }

    #[test]
    fn affects_channel() {
        let ch1 = ChannelId::new();
        let ch2 = ChannelId::new();
        let scope = SignalScope::Channel(ch1);

        assert!(scope.affects(ch1));
        assert!(!scope.affects(ch2));
    }

    #[test]
    fn display() {
        let global = SignalScope::Global;
        assert_eq!(format!("{global}"), "scope:global");

        let ch = ChannelId::new();
        let channel = SignalScope::Channel(ch);
        assert!(format!("{channel}").starts_with("scope:ch:"));
    }

    #[test]
    fn equality() {
        let ch1 = ChannelId::new();
        let ch2 = ChannelId::new();

        assert_eq!(SignalScope::Global, SignalScope::Global);
        assert_eq!(SignalScope::Channel(ch1), SignalScope::Channel(ch1));
        assert_ne!(SignalScope::Channel(ch1), SignalScope::Channel(ch2));
        assert_ne!(SignalScope::Global, SignalScope::Channel(ch1));
    }
}

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
///   └── WithChildren (channel + descendants)
///         └── Channel (specific only)
/// ```
///
/// # Permission Requirements
///
/// | Scope | Standard Privilege | Elevated Privilege |
/// |-------|-------------------|-------------------|
/// | `Global` | Denied | Allowed |
/// | `WithChildren` (own) | Allowed | Allowed |
/// | `WithChildren` (other) | Denied | Allowed |
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
/// let with_children = SignalScope::WithChildren(ChannelId::new());
///
/// assert!(global.is_global());
/// assert!(!channel.is_global());
/// assert!(with_children.includes_descendants());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalScope {
    /// Affects all channels and components.
    ///
    /// **Requires elevated privilege.**
    Global,

    /// Affects a specific channel only.
    Channel(ChannelId),

    /// Affects a channel and all its descendants.
    ///
    /// Use this when you want to signal a subtree of channels.
    /// Descendant checking requires World access at runtime.
    WithChildren(ChannelId),
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

    /// Returns `true` if this scope includes descendants.
    ///
    /// Both `Global` and `WithChildren` include descendants.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::{SignalScope, ChannelId};
    ///
    /// assert!(SignalScope::Global.includes_descendants());
    /// assert!(SignalScope::WithChildren(ChannelId::new()).includes_descendants());
    /// assert!(!SignalScope::Channel(ChannelId::new()).includes_descendants());
    /// ```
    #[must_use]
    pub fn includes_descendants(&self) -> bool {
        matches!(self, Self::Global | Self::WithChildren(_))
    }

    /// Returns `true` if this scope affects the given channel (without descendant check).
    ///
    /// For `WithChildren`, this only checks the root channel.
    /// Use `affects_with_descendants()` for full descendant checking.
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
            Self::Channel(id) | Self::WithChildren(id) => *id == channel,
        }
    }

    /// Checks if this scope affects the given channel, with descendant support.
    ///
    /// The `is_descendant` closure is called for `WithChildren` scope to check
    /// if the channel is a descendant of the scope's root.
    ///
    /// # Arguments
    ///
    /// * `channel` - The channel to check
    /// * `is_descendant` - Closure that returns `true` if `channel` is a descendant of the scope root
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::{SignalScope, ChannelId};
    ///
    /// let root = ChannelId::new();
    /// let child = ChannelId::new();
    /// let other = ChannelId::new();
    ///
    /// let scope = SignalScope::WithChildren(root);
    ///
    /// // Simulate: child is descendant of root, other is not
    /// assert!(scope.affects_with_descendants(root, |_| false)); // root itself
    /// assert!(scope.affects_with_descendants(child, |ch| ch == child)); // child
    /// assert!(!scope.affects_with_descendants(other, |_| false)); // unrelated
    /// ```
    #[must_use]
    pub fn affects_with_descendants<F>(&self, channel: ChannelId, is_descendant: F) -> bool
    where
        F: FnOnce(ChannelId) -> bool,
    {
        match self {
            Self::Global => true,
            Self::Channel(id) => *id == channel,
            Self::WithChildren(id) => *id == channel || is_descendant(channel),
        }
    }

    /// Returns the target channel if this is a Channel or WithChildren scope.
    #[must_use]
    pub fn channel(&self) -> Option<ChannelId> {
        match self {
            Self::Global => None,
            Self::Channel(id) | Self::WithChildren(id) => Some(*id),
        }
    }
}

impl std::fmt::Display for SignalScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "scope:global"),
            Self::Channel(id) => write!(f, "scope:ch:{}", id.uuid()),
            Self::WithChildren(id) => write!(f, "scope:ch+children:{}", id.uuid()),
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
        assert!(scope.includes_descendants());
        assert!(scope.channel().is_none());
    }

    #[test]
    fn channel_scope() {
        let ch = ChannelId::new();
        let scope = SignalScope::Channel(ch);
        assert!(!scope.is_global());
        assert!(!scope.includes_descendants());
        assert_eq!(scope.channel(), Some(ch));
    }

    #[test]
    fn with_children_scope() {
        let ch = ChannelId::new();
        let scope = SignalScope::WithChildren(ch);
        assert!(!scope.is_global());
        assert!(scope.includes_descendants());
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
    fn affects_with_children_root() {
        let root = ChannelId::new();
        let other = ChannelId::new();
        let scope = SignalScope::WithChildren(root);

        // affects() only checks root
        assert!(scope.affects(root));
        assert!(!scope.affects(other));
    }

    #[test]
    fn affects_with_descendants_global() {
        let ch = ChannelId::new();
        let global = SignalScope::Global;

        // Global always affects, closure not called
        assert!(global.affects_with_descendants(ch, |_| panic!("should not be called")));
    }

    #[test]
    fn affects_with_descendants_channel() {
        let ch1 = ChannelId::new();
        let ch2 = ChannelId::new();
        let scope = SignalScope::Channel(ch1);

        // Channel scope ignores descendants
        assert!(scope.affects_with_descendants(ch1, |_| panic!("should not be called")));
        assert!(!scope.affects_with_descendants(ch2, |_| true)); // even if closure says true
    }

    #[test]
    fn affects_with_descendants_with_children() {
        let root = ChannelId::new();
        let child = ChannelId::new();
        let other = ChannelId::new();
        let scope = SignalScope::WithChildren(root);

        // Root itself
        assert!(scope.affects_with_descendants(root, |_| false));

        // Child (closure returns true)
        assert!(scope.affects_with_descendants(child, |ch| ch == child));

        // Unrelated (closure returns false)
        assert!(!scope.affects_with_descendants(other, |_| false));
    }

    #[test]
    fn display() {
        let global = SignalScope::Global;
        assert_eq!(format!("{global}"), "scope:global");

        let ch = ChannelId::new();
        let channel = SignalScope::Channel(ch);
        assert!(format!("{channel}").starts_with("scope:ch:"));

        let with_children = SignalScope::WithChildren(ch);
        assert!(format!("{with_children}").starts_with("scope:ch+children:"));
    }

    #[test]
    fn equality() {
        let ch1 = ChannelId::new();
        let ch2 = ChannelId::new();

        assert_eq!(SignalScope::Global, SignalScope::Global);
        assert_eq!(SignalScope::Channel(ch1), SignalScope::Channel(ch1));
        assert_eq!(
            SignalScope::WithChildren(ch1),
            SignalScope::WithChildren(ch1)
        );
        assert_ne!(SignalScope::Channel(ch1), SignalScope::Channel(ch2));
        assert_ne!(
            SignalScope::WithChildren(ch1),
            SignalScope::WithChildren(ch2)
        );
        assert_ne!(SignalScope::Global, SignalScope::Channel(ch1));
        assert_ne!(SignalScope::Channel(ch1), SignalScope::WithChildren(ch1));
    }
}
